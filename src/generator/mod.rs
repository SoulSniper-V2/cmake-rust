//! Build-system generation (port of cmGlobalGenerator::Generate and the
//! cmGeneratorTarget / cmLocalGenerator / Ninja generator layer).
//!
//! Structure mirrors the C++ sources so work can proceed in parallel:
//! - `ctx.rs`      GenCtx: read-only access to the configured state + caches
//!   (cmGlobalGenerator/cmLocalGenerator queries, genex hook).
//! - `target.rs`   cmGeneratorTarget core: names, directories, artifacts,
//!   sources, linker language.            (lead)
//! - `options.rs`  compile definitions/options/includes/features/link
//!   options/directories with usage requirements (uses genex).
//! - `flags.rs`    cmLocalGenerator flag assembly: AddLanguageFlags,
//!   standards, PIC, visibility, include/define formatting,
//!   cmRulePlaceholderExpander.
//! - `link.rs`     cmComputeLinkDepends + cmComputeLinkInformation (link
//!   line items, search paths, rpaths).
//! - `custom.rs`   cmCustomCommandGenerator.
//! - `ninja/`      cmGlobalNinjaGenerator, cmLocalNinjaGenerator,
//!   cmNinjaTargetGenerator + Normal/Utility.  (lead)
//! - `install.rs`  cmInstallGenerator family -> cmake_install.cmake.
//! - `tests.rs`    cmTestGenerator -> CTestTestfile.cmake.
//! - `export.rs`   export()/install(EXPORT) files.

mod autogen;
pub mod automatic;
mod compatible;
pub mod ctx;
pub mod custom;
pub mod evaluation_files;
pub mod export;
pub mod extra;
pub mod flags;
pub mod genex_host;
pub mod graphviz;
pub mod install;
mod install_name;
pub(crate) mod ispc;
pub mod link;
mod module_support;
pub mod ninja;
pub mod osx_bundle;
pub mod package_info;
pub mod pch;
pub mod target;
pub mod tests;
pub mod trace;
mod verify_header_sets;

#[cfg(test)]
mod oracle_tests;

use std::collections::BTreeSet;

use crate::configure::ConfigureOptions;
use crate::interp::Interp;

/// `cmake::Generate`: compute and write the build system. Returns 0 on
/// success.
pub fn generate(interp: &mut Interp, options: &ConfigureOptions) -> i32 {
    let multi_config = interp.get_def("CMAKE_GENERATOR") == Some("Ninja Multi-Config");
    ninja::MULTI_CONFIG.set(multi_config);
    if multi_config {
        // cmGlobalNinjaMultiGenerator::InspectConfigTypeVariables.
        let mut config_list: Vec<String> = interp
            .get_def("CMAKE_CONFIGURATION_TYPES")
            .map(|value| crate::util::expand_list(value, true))
            .unwrap_or_default();
        // cmMakefile::GetGeneratorConfigs(IncludeEmptyConfig).
        if config_list.is_empty() {
            config_list.push(String::new());
        }
        let configs: BTreeSet<String> = config_list.iter().cloned().collect();
        let fatal = |interp: &mut Interp, message: String| {
            interp.issue_at(crate::messages::MessageType::FatalError, &message, &[]);
        };
        // cmGlobalNinjaGenerator::ListSubsetWithAll.
        fn subset_with_all(
            all: &BTreeSet<String>,
            defaults: &BTreeSet<String>,
            items: &[String],
        ) -> Option<BTreeSet<String>> {
            let mut result = BTreeSet::new();
            for item in items {
                if item == "all" {
                    if items.len() == 1 {
                        result = defaults.clone();
                    } else {
                        return None;
                    }
                } else if all.contains(item) {
                    result.insert(item.clone());
                } else {
                    return None;
                }
            }
            Some(result)
        }
        let mut default_file_config = interp
            .get_def("CMAKE_DEFAULT_BUILD_TYPE")
            .unwrap_or_default()
            .to_owned();
        if default_file_config.is_empty() {
            default_file_config = config_list[0].clone();
        }
        if !configs.contains(&default_file_config) {
            fatal(
                interp,
                format!(
                    "The configuration specified by CMAKE_DEFAULT_BUILD_TYPE ({default_file_config}) is not present in CMAKE_CONFIGURATION_TYPES"
                ),
            );
            return -1;
        }
        let cross_list: Vec<String> = interp
            .get_def("CMAKE_CROSS_CONFIGS")
            .map(|value| crate::util::expand_list(value, true))
            .unwrap_or_default();
        let Some(cross_configs) = subset_with_all(&configs, &configs, &cross_list) else {
            fatal(
                interp,
                "CMAKE_CROSS_CONFIGS is not a subset of CMAKE_CONFIGURATION_TYPES".to_owned(),
            );
            return -1;
        };
        let mut default_configs_string = interp
            .get_def("CMAKE_DEFAULT_CONFIGS")
            .unwrap_or_default()
            .to_owned();
        if default_configs_string.is_empty() {
            default_configs_string = default_file_config.clone();
        }
        if !default_configs_string.is_empty()
            && default_configs_string != default_file_config
            && (default_file_config.is_empty() || cross_configs.is_empty())
        {
            fatal(
                interp,
                "CMAKE_DEFAULT_CONFIGS cannot be used without CMAKE_DEFAULT_BUILD_TYPE or CMAKE_CROSS_CONFIGS"
                    .to_owned(),
            );
            return -1;
        }
        let mut default_configs = BTreeSet::new();
        if !default_file_config.is_empty() {
            let mut superset = cross_configs.clone();
            superset.insert(default_file_config.clone());
            let items = crate::util::expand_list(&default_configs_string, true);
            let Some(subset) = subset_with_all(&superset, &cross_configs, &items) else {
                fatal(
                    interp,
                    "CMAKE_DEFAULT_CONFIGS is not a subset of CMAKE_CROSS_CONFIGS".to_owned(),
                );
                return -1;
            };
            default_configs = subset;
        }
        interp.ext.multi_config = true;
        interp.ext.gen_configs = config_list;
        interp.ext.gen_default_file_config = default_file_config;
        interp.ext.gen_cross_configs = cross_configs;
        interp.ext.gen_default_configs = default_configs;
    } else {
        for name in [
            "CMAKE_DEFAULT_BUILD_TYPE",
            "CMAKE_CROSS_CONFIGS",
            "CMAKE_DEFAULT_CONFIGS",
        ] {
            if interp.get_def(name).is_some() {
                interp.issue_at(
                    crate::messages::MessageType::FatalError,
                    &format!(
                        "Generator\n  Ninja\ndoes not support variable\n  {name}\nbut it has been specified."
                    ),
                    &[],
                );
                return -1;
            }
        }
    }
    let start = std::time::Instant::now();
    finalize_target_configuration(interp);
    // Generator-added targets (build-database merge targets, header-set
    // verification targets, autogen) emit after each directory's global
    // targets; this boundary is the end of configure-time target creation.
    interp.ext.generated_targets_start = Some(interp.ext.target_order.len());
    if !check_build_database_targets(interp) {
        return -1;
    }
    // cmGlobalGenerator::CreateFileGenerateOutputs: file(GENERATE) outputs
    // become generated sources before any target computation sees them.
    evaluation_files::create_outputs(interp);
    // cmGlobalGenerator::Compute -> cmMakefile::Generate: deferred custom
    // command actions attach to sources/targets before anything is computed.
    crate::commands::custom_actions::run_generator_actions(interp, &mut expand_output_genex);
    drop_transient_list_files(interp);
    // cmGlobalGenerator::CreateFileGenerateOutputs: file(GENERATE) outputs
    // become generated sources before target source resolution runs.
    evaluation_files::create_outputs(interp);
    if !verify_header_sets::add_targets(interp) {
        return -1;
    }
    // cmQtAutoGenGlobalInitializer::InitializeCustomTargets.
    let Some(autogen) = autogen::initialize(interp) else {
        return -1;
    };
    // cmGlobalGenerator::Compute: ApplyCXXStdTargets and
    // DiscoverSyntheticTargets follow ComputeTargetCompileFeatures (lazy in
    // this implementation).
    let mut module_diagnostics = ninja::modules::finalize_cxx_module_targets(interp);
    // cmGlobalGenerator::AddAutomaticSources: unity builds and PCH.
    automatic::add_automatic_sources(interp);
    // cmMakefile::GenerateAfterGeneratorTargets: the build-database merge
    // commands attach once every generator target has registered its
    // database path, and before TraceDependencies resolves their produced
    // `.rule` sources into the merge utility targets.
    run_build_database_actions(interp, &mut expand_output_genex);
    // cmLocalGenerator::TraceDependencies for every directory.
    trace::trace_dependencies(interp);
    let (computed, mut diagnostics) = {
        let ctx = ctx::GenCtx::new(interp);
        let (computed, generated) = ctx.check_and_generate(&autogen);
        module_diagnostics.extend(generated);
        (computed, module_diagnostics)
    };
    issue_diagnostics(interp, std::mem::take(&mut diagnostics));
    if !computed {
        return -1;
    }
    if interp.debug.output && !options.in_try_compile {
        // cmGlobalGenerator::Generate reports progress after each local
        // generator, with that directory as the current makefile, whose
        // call stack is its own list file. (Printed once generation has
        // finished, so error output is not interleaved the same way.)
        let dirs: Vec<(String, String)> = interp
            .makefiles
            .iter()
            .map(|mf| {
                (
                    mf.binary_dir.clone(),
                    interp.project_list_file(&mf.source_dir),
                )
            })
            .collect();
        for (binary, list) in dirs {
            interp.stdout_line(&format!(
                "-- Generating {binary}\n   Called from: [1]\t{list}"
            ));
        }
    }
    if !options.in_try_compile {
        let elapsed = start.elapsed().as_secs_f64();
        crate::commands::core::status_line(interp, &format!("Generating done ({elapsed:.1}s)"));
    }
    if let Some(file) = &options.graphviz {
        println!("Generate graphviz: {file}");
        let ctx = ctx::GenCtx::new(interp);
        graphviz::generate_graphviz(&ctx, file);
    }
    if !options.in_try_compile {
        crate::configure::check_for_unused_variables(interp);
    }
    // cmake::Generate: "Generating done" precedes the error-flag check.
    if interp.error {
        return -1;
    }
    if let Err(error) = install::write_install_index(&ctx::GenCtx::new(interp)) {
        crate::configure::system_error(
            Some(interp),
            &format!("Unable to write InstallScripts.json: {error}"),
        );
        return -1;
    }
    0
}

/// The end of `cmMakefile::DoGenerate`: list files that no longer exist
/// (or live under CMakeTmp) are transient and cannot trigger a re-run.
fn drop_transient_list_files(interp: &mut Interp) {
    let not_persistent =
        |path: &str| path.contains("CMakeTmp") || !std::path::Path::new(path).exists();
    for makefile in &mut interp.makefiles {
        makefile.list_files.retain(|path| !not_persistent(path));
    }
    if let Some(deps) = interp.ext.misc.get("cmake-depend-files") {
        let kept: Vec<String> = crate::util::expand_list(deps, false)
            .into_iter()
            .filter(|path| !not_persistent(path))
            .collect();
        interp
            .ext
            .misc
            .insert("cmake-depend-files".to_owned(), kept.join(";"));
    }
}

/// `cmGlobalGenerator::FinalizeTargetConfiguration` +
/// `cmTarget::FinalizeTargetConfiguration`: append each directory's
/// COMPILE_DEFINITIONS to its targets and the current-dir interface
/// includes when CMAKE_INCLUDE_CURRENT_DIR_IN_INTERFACE is on.
fn finalize_target_configuration(interp: &mut Interp) {
    use crate::state::TargetKind;
    let keys: Vec<String> = interp.ext.target_order.clone();
    for key in keys {
        let Some(target) = interp.ext.targets.get(&key) else {
            continue;
        };
        if target.kind == TargetKind::GlobalTarget {
            continue;
        }
        // cmTarget::FinalizeTargetConfiguration: CheckLinkLibraryPattern —
        // a raw internal tag (<LINK_LIBRARY:...>, </LINK_LIBRARY:...>,
        // <LINK_GROUP:...>, </LINK_GROUP:...>) appearing as a whole item in
        // LINK_LIBRARIES, INTERFACE_LINK_LIBRARIES or
        // INTERFACE_LINK_LIBRARIES_DIRECT is a FATAL_ERROR at the entry's
        // backtrace, and the target's remaining finalize work is skipped.
        let mut issues = Vec::new();
        for property in [
            "LINK_LIBRARIES",
            "INTERFACE_LINK_LIBRARIES",
            "INTERFACE_LINK_LIBRARIES_DIRECT",
        ] {
            let Some(entries) = target.provenance.get(property) else {
                continue;
            };
            for (value, backtrace) in entries {
                for item in crate::util::expand_list(value, false) {
                    let Some(kind) = link_tag(&item) else {
                        continue;
                    };
                    issues.push((
                        format!("Property {property} contains the invalid item \"{item}\". The {property} property may contain the generator-expression \"$<LINK_{kind}:...>\" which may be used to specify how the libraries are linked."),
                        backtrace.clone(),
                    ));
                }
            }
        }
        let invalid = !issues.is_empty();
        let imported = target.imported;
        let dir = target.makefile;
        let kind = target.kind;
        let exports = target
            .props
            .get("ENABLE_EXPORTS")
            .is_some_and(crate::util::is_on);
        for (message, backtrace) in issues {
            interp.issue_at(
                crate::messages::MessageType::FatalError,
                &message,
                &backtrace,
            );
        }
        if invalid || imported {
            continue;
        }
        // AppendBuildInterfaceIncludes
        let interface_includes = matches!(
            kind,
            TargetKind::SharedLibrary
                | TargetKind::StaticLibrary
                | TargetKind::ModuleLibrary
                | TargetKind::InterfaceLibrary
        ) || (kind == TargetKind::Executable && exports);
        let include_current = interp
            .dir_final_vars
            .get(dir)
            .and_then(|vars| {
                vars.get("CMAKE_INCLUDE_CURRENT_DIR_IN_INTERFACE")
                    .cloned()
                    .flatten()
            })
            .is_some_and(|value| crate::util::is_on(&value));
        let definitions =
            crate::commands::target::directory_entries(interp, dir, "COMPILE_DEFINITIONS");
        let binary = interp.makefiles[dir].binary_dir.clone();
        let source = interp.makefiles[dir].source_dir.clone();
        let Some(target) = interp.ext.targets.get_mut(&key) else {
            continue;
        };
        if interface_includes && include_current {
            let value = format!("$<BUILD_INTERFACE:{binary};{source}>");
            target
                .props
                .append("INTERFACE_INCLUDE_DIRECTORIES", &value, false);
            target
                .provenance
                .entry("INTERFACE_INCLUDE_DIRECTORIES".into())
                .or_default()
                .push((value, Vec::new()));
        }
        if kind == TargetKind::InterfaceLibrary {
            continue;
        }
        for (value, backtrace) in definitions {
            if value.is_empty() {
                continue;
            }
            target.props.append("COMPILE_DEFINITIONS", &value, false);
            target
                .provenance
                .entry("COMPILE_DEFINITIONS".into())
                .or_default()
                .push((value, backtrace));
        }
    }
}

/// `CheckLinkLibraryPattern`'s tag shape: `</?LINK_(LIBRARY|GROUP):[^;>]*>`
/// as a whole list item (entries are expanded first, so `;` cannot occur).
/// Returns the `LIBRARY`/`GROUP` kind used by the diagnostic.
fn link_tag(item: &str) -> Option<&'static str> {
    let body = item.strip_prefix('<')?.strip_suffix('>')?;
    let body = body.strip_prefix('/').unwrap_or(body);
    let (kind, rest) = body.strip_prefix("LINK_")?.split_once(':')?;
    if rest.contains('>') {
        return None;
    }
    match kind {
        "LIBRARY" => Some("LIBRARY"),
        "GROUP" => Some("GROUP"),
        _ => None,
    }
}

/// `cmLocalGenerator::ExpandCustomCommandOutputGenex` (single config): the
/// unique full paths of a genex custom command output.
fn expand_output_genex(
    interp: &Interp,
    dir: usize,
    output: &str,
    backtrace: &[crate::messages::Frame],
) -> Result<Vec<String>, String> {
    let ctx = ctx::GenCtx::new(interp);
    let binary = &interp.makefiles[dir].binary_dir;
    let mut paths: Vec<String> = Vec::new();
    // GetGeneratorConfigs(IncludeEmptyConfig): every configuration's paths,
    // deduplicated in first-seen order.
    for config in &ctx.configs {
        let view = ctx.for_config(config);
        // cmLocalGenerator::ExpandCustomCommandOutputPaths evaluates the
        // whole output as one generator expression (no OUTPUT_CONFIG
        // stripping).
        let value = view.genex_output(output, dir, backtrace);
        if let Some(message) = view.errors.borrow().first().map(|d| d.text().to_owned()) {
            return Err(message);
        }
        for path in crate::util::expand_list(&value, false) {
            let path = crate::util::collapse_full_path(&path, binary);
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    Ok(paths)
}

/// `cmGlobalGenerator::AddBuildDatabaseTargets` +
/// `cmGlobalGenerator::AddMergeTarget`: the reserved-name checks, utility
/// targets, and merge commands for `CMAKE_EXPORT_BUILD_DATABASE`. The merge
/// commands are `GeneratorActionWhen::AfterGeneratorTargets` actions —
/// recorded in `ext.build_database_merges` and resolved by
/// [`run_build_database_actions`] once every generator target (including
/// synthetic C++ module targets) has registered its database path.
/// Without the experimental gate upstream fails generation silently
/// (`return {};`), which is mirrored.
/// Reports diagnostics a generation context collected.
fn issue_diagnostics(interp: &mut Interp, diagnostics: Vec<ctx::Diagnostic>) {
    for diagnostic in diagnostics {
        match diagnostic {
            ctx::Diagnostic::Message(kind, backtrace, message) => {
                interp.issue_at(kind, &message, &backtrace);
            }
            // `cmSystemTools::Error`: printed without any location or
            // reflow, and marks the generation failed.
            ctx::Diagnostic::Bare(message) => {
                interp.error = true;
                interp.stderr_line(&format!("CMake Error: {message}"));
            }
        }
    }
}

fn check_build_database_targets(interp: &mut Interp) -> bool {
    enum Gate {
        Skip,
        Fail,
        Run,
    }
    let (gate, configs, diagnostics) = {
        let ctx = ctx::GenCtx::new(interp);
        let requested = ctx
            .var(0, "CMAKE_EXPORT_BUILD_DATABASE")
            .is_some_and(|value| crate::util::is_on(&value));
        let gate = if !requested {
            Gate::Skip
        } else if !ctx.experimental_enabled(0, "ExportBuildDatabase") {
            Gate::Fail
        } else {
            Gate::Run
        };
        // cmMakefile::GetGeneratorConfigs(ExcludeEmptyConfig).
        let configs = ctx
            .configs
            .iter()
            .filter(|config| !config.is_empty())
            .cloned()
            .collect::<Vec<_>>();
        // The experimental feature's author warning.
        let diagnostics = std::mem::take(&mut *ctx.errors.borrow_mut());
        (gate, configs, diagnostics)
    };
    issue_diagnostics(interp, diagnostics);
    match gate {
        Gate::Skip => return true,
        Gate::Fail => return false,
        Gate::Run => {}
    }

    // cmGlobalGenerator::CheckReservedTargetName{,Prefix} over the target
    // search index: the targets that are not imported or are imported
    // globally (cmGlobalGenerator::IndexTarget).
    const REASON: &str = "when exporting build databases are enabled";
    let mut ok = true;
    let mut reserved: Vec<(String, Vec<crate::messages::Frame>)> = Vec::new();
    for target in interp.ext.targets.values() {
        if (!target.imported || target.imported_global)
            && target.name.starts_with("cmake_build_database-")
        {
            reserved.push((target.name.clone(), target.backtrace.clone()));
        }
    }
    // The reserved names are checked first, and stop before the prefixes.
    if let Some((_, target)) = crate::state::find_target_entry(interp, "cmake_build_database") {
        let backtrace = target.backtrace.clone();
        interp.issue_at(
            crate::messages::MessageType::FatalError,
            &format!("The target name \"cmake_build_database\" is reserved {REASON}."),
            &backtrace,
        );
        return false;
    }
    // The prefix check walks the name-sorted target search index.
    reserved.sort_by(|(a, _), (b, _)| a.cmp(b));
    for (name, backtrace) in reserved {
        interp.issue_at(
            crate::messages::MessageType::FatalError,
            &format!("The target name \"{name}\" is reserved {REASON}."),
            &backtrace,
        );
        ok = false;
    }
    if !ok {
        return false;
    }

    let home_binary = interp.home_binary.clone();

    // cmGlobalGenerator::AddMergeTarget.
    let add_merge_target = |interp: &mut Interp,
                            name: &str,
                            comment: String,
                            output: String,
                            inputs: crate::state::BuildDatabaseInputs| {
        // ModuleCompilationDatabaseCommandAction: the merge runs after
        // generator targets so its input list sees every registered path.
        crate::commands::target::mark_generated(interp, 0, &output);
        interp
            .ext
            .build_database_merges
            .push(crate::state::BuildDatabaseMerge {
                comment,
                output: output.clone(),
                inputs,
            });

        // ModuleCompilationDatabaseTargetAction: a utility target whose
        // custom command only depends on the merge output.
        let mut target = crate::state::Target::new(name, crate::state::TargetKind::Utility, 0);
        target.props.set("EXCLUDE_FROM_ALL", "TRUE".to_owned());
        let key = crate::state::register_target(interp, target);
        let command = crate::state::CustomCommand {
            makefile: 0,
            depends: vec![output],
            working_directory: home_binary.clone(),
            ..Default::default()
        };
        interp
            .ext
            .generator_actions
            .push(crate::state::GeneratorAction::Utility {
                target: key,
                command,
            });
    };

    // Per-configuration targets, per language.
    for config in &configs {
        let mut all_config_paths = Vec::new();
        for &lang in MODULE_DATABASE_LANGUAGES {
            let comment = format!("Combining module command databases for {lang} and {config}");
            let output = format!("{home_binary}/build_database_{lang}_{config}.json");
            add_merge_target(
                interp,
                &format!("cmake_build_database-{lang}-{config}"),
                comment,
                output.clone(),
                crate::state::BuildDatabaseInputs::PerConfig {
                    config: config.clone(),
                    lang: (*lang).to_owned(),
                },
            );
            all_config_paths.push(output);
        }
        let comment = format!("Combining module command databases for {config}");
        let output = format!("{home_binary}/build_database_{config}.json");
        add_merge_target(
            interp,
            &format!("cmake_build_database-{config}"),
            comment,
            output,
            crate::state::BuildDatabaseInputs::List(all_config_paths),
        );
    }

    // Per-language targets across all configurations.
    let mut all_config_paths = Vec::new();
    for &lang in MODULE_DATABASE_LANGUAGES {
        let comment = format!("Combining module command databases for {lang}");
        let output = format!("{home_binary}/build_database_{lang}.json");
        add_merge_target(
            interp,
            &format!("cmake_build_database-{lang}"),
            comment,
            output.clone(),
            crate::state::BuildDatabaseInputs::PerLanguage {
                lang: (*lang).to_owned(),
            },
        );
        all_config_paths.push(output);
    }

    // The overall target.
    add_merge_target(
        interp,
        "cmake_build_database",
        "Combining all module command databases".to_owned(),
        format!("{home_binary}/build_database.json"),
        crate::state::BuildDatabaseInputs::List(all_config_paths),
    );

    true
}

/// The `GeneratorActionWhen::AfterGeneratorTargets` pass: each generator
/// target's constructor registers its build-database path
/// (`cmGeneratorTarget::BuildDatabasePath` +
/// `cmGlobalGenerator::AddBuildDatabaseFile`), then the merge command
/// actions attach their custom commands.
fn run_build_database_actions(
    interp: &mut Interp,
    expand: &mut crate::commands::custom_actions::ExpandOutputGenex,
) {
    let mut registrations: Vec<(usize, String, String)> = Vec::new();
    let diagnostics = {
        let ctx = ctx::GenCtx::new(interp);
        let configs: Vec<&String> = ctx
            .configs
            .iter()
            .filter(|config| !config.is_empty())
            .collect();
        for key in &interp.ext.target_order {
            let Some(target) = interp.ext.targets.get(key) else {
                continue;
            };
            // cmGeneratorTarget::BuildDatabasePath for the CXX language.
            if !target
                .props
                .get("EXPORT_BUILD_DATABASE")
                .is_some_and(crate::util::is_on)
                || !ctx.experimental_enabled(target.makefile, "ExportBuildDatabase")
            {
                continue;
            }
            for config in &configs {
                let object_dir = target::object_directory(&ctx, target, config);
                registrations.push((
                    target.makefile,
                    (*config).clone(),
                    format!(
                        "{}/CXX_build_database.json",
                        object_dir.trim_end_matches('/')
                    ),
                ));
            }
        }
        // The experimental feature's author warning.
        std::mem::take(&mut *ctx.errors.borrow_mut())
    };
    issue_diagnostics(interp, diagnostics);
    for (dir, config, path) in registrations {
        crate::commands::target::mark_generated(interp, dir, &path);
        interp
            .ext
            .per_config_module_dbs
            .entry(config)
            .or_default()
            .entry("CXX".to_owned())
            .or_default()
            .push(path.clone());
        interp
            .ext
            .per_language_module_dbs
            .entry("CXX".to_owned())
            .or_default()
            .push(path);
    }

    // ModuleCompilationDatabaseCommandAction for each merge target.
    let merges = std::mem::take(&mut interp.ext.build_database_merges);
    let cmake = interp.cmake_command.clone();
    let home_binary = interp.home_binary.clone();
    for merge in merges {
        let inputs = match merge.inputs {
            crate::state::BuildDatabaseInputs::PerConfig { config, lang } => interp
                .ext
                .per_config_module_dbs
                .get(&config)
                .and_then(|langs| langs.get(&lang))
                .cloned()
                .unwrap_or_default(),
            crate::state::BuildDatabaseInputs::PerLanguage { lang } => interp
                .ext
                .per_language_module_dbs
                .get(&lang)
                .cloned()
                .unwrap_or_default(),
            crate::state::BuildDatabaseInputs::List(list) => list,
        };
        let mut command = crate::state::CustomCommand {
            makefile: 0,
            ..Default::default()
        };
        command.command_lines = vec![
            [
                cmake.clone(),
                "-E".to_owned(),
                "cmake_module_compile_db".to_owned(),
                "merge".to_owned(),
                "-o".to_owned(),
                merge.output.clone(),
            ]
            .into_iter()
            .chain(inputs.iter().cloned())
            .collect(),
        ];
        command.outputs = vec![merge.output];
        if !inputs.is_empty() {
            command.main_dependency = inputs[0].clone();
        }
        // cmCustomCommand::SetDepends re-inserts the existing main
        // dependency at the head of the list, so `inputs[0]` is duplicated.
        command.depends = inputs
            .iter()
            .take(1)
            .cloned()
            .chain(inputs.iter().cloned())
            .collect();
        command.working_directory = home_binary.clone();
        command.comment = Some(merge.comment);
        command.depends_explicit_only = true;
        crate::commands::custom_actions::run_action(
            interp,
            expand,
            crate::state::GeneratorAction::Output {
                command,
                replace: false,
            },
        );
    }
}

/// The languages `cmGlobalGenerator::AddBuildDatabaseTargets` merges
/// module compilation databases for.
const MODULE_DATABASE_LANGUAGES: &[&str] = &["CXX"];
