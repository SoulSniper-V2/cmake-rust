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

pub mod automatic;
mod compatible;
pub mod ctx;
pub mod custom;
pub mod evaluation_files;
pub mod export;
pub mod flags;
pub mod genex_host;
pub mod graphviz;
pub mod install;
pub mod link;
pub mod ninja;
pub mod package_info;
pub mod pch;
pub mod target;
pub mod tests;
pub mod trace;
mod verify_header_sets;

#[cfg(test)]
mod oracle_tests;

use crate::configure::ConfigureOptions;
use crate::interp::Interp;

/// `cmake::Generate`: compute and write the build system. Returns 0 on
/// success.
pub fn generate(interp: &mut Interp, options: &ConfigureOptions) -> i32 {
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
    let start = std::time::Instant::now();
    finalize_target_configuration(interp);
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
    // cmGlobalGenerator::AddAutomaticSources: unity builds and PCH.
    automatic::add_automatic_sources(interp);
    // cmLocalGenerator::TraceDependencies for every directory.
    trace::trace_dependencies(interp);
    let diagnostics = {
        let ctx = ctx::GenCtx::new(interp);
        ctx.check_and_generate()
    };
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
        if target.imported || target.kind == TargetKind::GlobalTarget {
            continue;
        }
        let dir = target.makefile;
        let kind = target.kind;
        let exports = target
            .props
            .get("ENABLE_EXPORTS")
            .is_some_and(crate::util::is_on);
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

/// `cmLocalGenerator::ExpandCustomCommandOutputGenex` (single config): the
/// unique full paths of a genex custom command output.
fn expand_output_genex(
    interp: &Interp,
    dir: usize,
    output: &str,
    backtrace: &[crate::messages::Frame],
) -> Result<Vec<String>, String> {
    let ctx = ctx::GenCtx::new(interp);
    // cmLocalGenerator::ExpandCustomCommandOutputPaths evaluates the whole
    // output as one generator expression (no OUTPUT_CONFIG stripping).
    let value = ctx.genex_output(output, dir, backtrace);
    if let Some(message) = ctx.errors.borrow().first().map(|d| d.text().to_owned()) {
        return Err(message);
    }
    let binary = &interp.makefiles[dir].binary_dir;
    let mut paths: Vec<String> = Vec::new();
    for path in crate::util::expand_list(&value, false) {
        let path = crate::util::collapse_full_path(&path, binary);
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    Ok(paths)
}
