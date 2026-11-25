//! file(GENERATE) evaluation:
//!
//! - `create_outputs` — `cmLocalGenerator::CreateEvaluationFileOutputs` /
//!   `cmGeneratorExpressionEvaluationFile::CreateOutputFile`: during
//!   Compute, evaluate the OUTPUT expressions, mark the outputs as
//!   generated sources, and record the `FilenameTargetDepends` sets that
//!   `TraceDependencies` uses to reject dependency loops.
//! - `generate` — `cmGlobalGenerator::ProcessEvaluationFiles` /
//!   `cmGeneratorExpressionEvaluationFile::Generate`: read inputs,
//!   evaluate CONDITION/OUTPUT/CONTENT for every (language, config) pair,
//!   write the outputs, and report files produced by different commands.

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::PermissionsExt;

use super::ctx::GenCtx;
use crate::interp::Interp;
use crate::messages::{Frame, MessageType};
use crate::policy::{self, PolicyStatus};
use crate::state::EvaluationFile;
use crate::util;

/// `cmGeneratorExpressionEvaluationFile::FixRelativePath`: CMP0070 OLD and
/// WARN keep relative paths unchanged (they end up relative to the working
/// directory); NEW resolves INPUT against the current source directory and
/// OUTPUT against the current binary directory.
fn fix_relative_path(
    ctx: &GenCtx,
    file: &EvaluationFile,
    path: &str,
    for_input: bool,
    dir_backtrace: &[Frame],
    warn: bool,
) -> String {
    let makefile = &ctx.interp.makefiles[file.dir];
    match file.cmp0070 {
        PolicyStatus::Warn => {
            if warn {
                ctx.message(
                    MessageType::AuthorWarning,
                    dir_backtrace.to_vec(),
                    format!(
                        "{}\nfile(GENERATE) given relative {} path:\n  {}\nThis is not defined behavior unless CMP0070 is set to NEW.  For compatibility with older versions of CMake, the previous undefined behavior will be used.",
                        policy::policy_warning(70),
                        if for_input { "INPUT" } else { "OUTPUT" },
                        path,
                    ),
                );
            }
            path.to_owned()
        }
        PolicyStatus::Old => path.to_owned(),
        PolicyStatus::New => {
            let base = if for_input {
                &makefile.source_dir
            } else {
                &makefile.binary_dir
            };
            util::collapse_full_path(path, base)
        }
    }
}

/// `cmGeneratorExpressionEvaluationFile::GetInputFileName`.
fn input_file_name(
    ctx: &GenCtx,
    file: &EvaluationFile,
    dir_backtrace: &[Frame],
    warn: bool,
) -> String {
    if util::is_full_path(&file.input) {
        util::collapse_full_path(&file.input, "")
    } else {
        fix_relative_path(ctx, file, &file.input, true, dir_backtrace, warn)
    }
}

/// `cmGeneratorExpressionEvaluationFile::GetOutputFileName`: evaluate the
/// OUTPUT expression for `config`/`lang` and fix it up per CMP0070.
fn output_file_name(
    ctx: &GenCtx,
    file: &EvaluationFile,
    target: Option<&str>,
    language: Option<&str>,
    dir_backtrace: &[Frame],
) -> (String, BTreeSet<String>) {
    let outcome = ctx.genex_eval(
        &file.output,
        file.dir,
        target,
        language,
        false,
        &file.backtrace,
    );
    let name = if util::is_full_path(&outcome.value) {
        util::collapse_full_path(&outcome.value, "")
    } else {
        fix_relative_path(ctx, file, &outcome.value, false, dir_backtrace, true)
    };
    (name, outcome.source_sensitive_targets)
}

/// `cmGlobalGenerator::CreateFileGenerateOutputs`: runs during Compute,
/// before TraceDependencies and CheckTargetsForMissingSources, so the
/// outputs are recognized as generated sources when target sources are
/// resolved.
pub fn create_outputs(interp: &mut Interp) {
    if interp.ext.evaluation_files.is_empty() {
        return;
    }
    let mut diagnostics = Vec::new();
    let mut marked: Vec<(usize, String, BTreeSet<String>)> = Vec::new();
    let mut depend: Vec<(usize, String)> = Vec::new();
    {
        let ctx = GenCtx::new(interp);
        let languages = interp.langs.enabled.clone();
        // CreateEvaluationFileOutputs: per directory, configuration, file.
        for config in &ctx.configs {
            let view = ctx.for_config(config);
            for file in &interp.ext.evaluation_files {
                let dir_backtrace = ctx.dir_backtrace(file.dir);
                let target = if file.target.is_empty() {
                    None
                } else {
                    ctx.target_at(file.dir, &file.target)
                        .map(|target| ctx.target_key(target))
                };
                for language in &languages {
                    let (name, depends) = output_file_name(
                        &view,
                        file,
                        target.as_deref(),
                        Some(language.as_str()),
                        &dir_backtrace,
                    );
                    marked.push((file.dir, name, depends));
                }
            }
        }
        for file in &interp.ext.evaluation_files {
            // cmMakefile::AddCMakeDependFile: the input file joins the build
            // system's check files so edits re-run CMake. Resolve it here —
            // silently, since CreateOutputFile does not read the input —
            // where &mut access to the makefile exists.
            if !file.input_is_content {
                let dir_backtrace = ctx.dir_backtrace(file.dir);
                depend.push((file.dir, input_file_name(&ctx, file, &dir_backtrace, false)));
            }
        }
        marked.sort_by_key(|(dir, _, _)| *dir);
        diagnostics.extend(ctx.errors.borrow().iter().cloned());
    }
    for (dir, path) in depend {
        // The list file vector drives the Ninja "re-run cmake" rule.
        interp.makefiles[dir].list_files.push(path.clone());
        interp.ext.configure_dependencies.insert(path.clone());
        let deps = interp
            .ext
            .misc
            .entry("cmake-depend-files".to_string())
            .or_default();
        if !deps.is_empty() {
            deps.push(';');
        }
        deps.push_str(&path);
    }
    for (dir, path, depends) in marked {
        // cmMakefile::GetOrCreateGeneratedSource(name): the source file is
        // created with cmSourceFileLocationKind::Known, which resolves a
        // relative output name against the source directory (a CMP0070 OLD
        // output stays relative at this point). There is no build rule for
        // the file; MarkAsGenerated records it as generated.
        if crate::commands::target::has_genex(&path) {
            continue;
        }
        let index = crate::commands::target::source_index(interp, dir, &path, true);
        crate::commands::target::mark_source_generated(interp, dir, index);
        let resolved = {
            let source = &mut interp.ext.sources.get_mut(&dir).unwrap()[index];
            // sf->SetProperty("__CMAKE_GENERATED_BY_CMAKE", "1").
            source
                .props
                .set("__CMAKE_GENERATED_BY_CMAKE", "1".to_owned());
            source.location.path()
        };
        interp
            .ext
            .filename_target_depends
            .entry(resolved)
            .or_default()
            .extend(depends);
    }
    for diagnostic in diagnostics {
        match diagnostic {
            super::ctx::Diagnostic::Message(kind, backtrace, message) => {
                interp.issue_at(kind, &message, &backtrace);
            }
            super::ctx::Diagnostic::Bare(message) => {
                interp.error = true;
                interp.stderr_line(&format!("CMake Error: {message}"));
            }
        }
    }
}

/// `cmGlobalGenerator::ProcessEvaluationFiles` for all directories.
pub fn generate(ctx: &GenCtx) {
    let interp = ctx.interp;
    let languages: Vec<String> = interp.langs.enabled.clone();
    let mut generated: BTreeSet<String> = BTreeSet::new();
    for file in &interp.ext.evaluation_files {
        let dir_backtrace = ctx.dir_backtrace(file.dir);
        // cmGeneratorExpressionEvaluationFile::Generate(lg): read the input
        // once, before the (language, config) loop.
        let mut permissions = file.permissions;
        let content = if file.input_is_content {
            file.input.clone()
        } else {
            let input_name = input_file_name(ctx, file, &dir_backtrace, true);
            if permissions == 0
                && let Ok(meta) = std::fs::metadata(&input_name)
            {
                permissions = meta.permissions().mode() & 0o7777;
            }
            match std::fs::read(&input_name) {
                Ok(bytes) => {
                    let text = crate::bytes::decode(&bytes);
                    // GetLineFromStream joins lines with "\n" and keeps a
                    // trailing separator after the last line.
                    let mut out = String::new();
                    let mut sep = "";
                    let mut lines: Vec<&str> = text.split('\n').collect();
                    if lines.last() == Some(&"") {
                        lines.pop();
                    }
                    for line in lines {
                        out.push_str(sep);
                        out.push_str(line.strip_suffix('\r').unwrap_or(line));
                        sep = "\n";
                    }
                    out.push_str(sep);
                    out
                }
                Err(_) => {
                    ctx.error(
                        dir_backtrace.clone(),
                        format!("Evaluation file \"{input_name}\" cannot be read."),
                    );
                    continue;
                }
            }
        };
        let target = if file.target.is_empty() {
            None
        } else {
            ctx.target_at(file.dir, &file.target)
                .map(|target| ctx.target_key(target))
        };
        // `outputFiles`: names produced by this command's Generate, and the
        // content each carried — used to catch conflicting writes.
        let mut outputs: BTreeMap<String, String> = BTreeMap::new();
        // `geef->Files`: every output name produced, for the cross-command
        // conflict check below.
        let mut files: BTreeSet<String> = BTreeSet::new();
        for language in &languages {
            let language = Some(language.as_str());
            // GetGeneratorConfigs(IncludeEmptyConfig): the build type, or
            // every multi-config configuration.
            for config in &ctx.configs {
                let view = ctx.for_config(config);
                let ctx = &view;
                if !file.condition.is_empty() {
                    let result = ctx.genex_full(
                        &file.condition,
                        file.dir,
                        target.as_deref(),
                        language,
                        false,
                        &file.backtrace,
                    );
                    if result == "0" {
                        continue;
                    }
                    if result != "1" {
                        ctx.error(
                            dir_backtrace.clone(),
                            format!(
                                "Evaluation file condition \"{}\" did not evaluate to valid content. Got \"{result}\".",
                                file.condition
                            ),
                        );
                        continue;
                    }
                }
                let (output, _) =
                    output_file_name(ctx, file, target.as_deref(), language, &dir_backtrace);
                let value = ctx.genex_full(
                    &content,
                    file.dir,
                    target.as_deref(),
                    language,
                    false,
                    &file.backtrace,
                );
                if let Some(existing) = outputs.get(&output) {
                    if *existing != value {
                        ctx.error(
                            dir_backtrace.clone(),
                            format!(
                                "Evaluation file to be written multiple times with different content. This is generally caused by the content evaluating the configuration type, language, or location of object files:\n {output}"
                            ),
                        );
                    }
                    continue;
                }
                files.insert(output.clone());
                outputs.insert(output.clone(), value.clone());
                ctx.cmake_output_files
                    .borrow_mut()
                    .push((file.dir, output.clone()));
                let mut text = String::new();
                let mut lines: Vec<&str> = value.split('\n').collect();
                let trailing = lines.last() == Some(&"");
                if trailing {
                    lines.pop();
                }
                let count = lines.len();
                for (index, line) in lines.into_iter().enumerate() {
                    text.push_str(line);
                    let has_newline = index + 1 < count || trailing;
                    if !file.newline.is_empty() {
                        text.push_str(&file.newline);
                    } else if has_newline {
                        text.push('\n');
                    }
                }
                let path = std::path::Path::new(&output);
                if crate::fsutil::write_if_different(path, &text).is_err() {
                    ctx.error(
                        dir_backtrace.clone(),
                        format!("Could not open file for write in copy operation {output}"),
                    );
                    continue;
                }
                if permissions != 0 {
                    let _ = std::fs::set_permissions(
                        path,
                        std::fs::Permissions::from_mode(permissions),
                    );
                }
            }
        }
        // cmLocalGenerator::ProcessEvaluationFiles: outputs of different
        // commands must not collide.
        let intersection: Vec<String> = files.intersection(&generated).cloned().collect();
        if !intersection.is_empty() {
            let wrapped = intersection
                .iter()
                .map(|name| format!("\"{name}\""))
                .collect::<Vec<_>>()
                .join(" ");
            ctx.system_error(format!(
                "Files to be generated by multiple different commands: {wrapped}"
            ));
            return;
        }
        generated.extend(files);
    }
}
