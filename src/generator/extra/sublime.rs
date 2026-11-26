//! `cmExtraSublimeTextGenerator`: `<project>.sublime-project` files.

use super::{Shared, relative_path, write_file};
use crate::generator::ctx::GenCtx;
use crate::generator::ninja::GlobalTarget;
use crate::state::{Target, TargetKind};
use crate::util;

/// `cmExtraSublimeTextGenerator::BuildMakeCommand` (Ninja base).
fn build_make_command(s: &Shared, make: &str, makefile: &str, target: &str) -> String {
    let mut command = format!("\"{make}\"");
    let generator = s.var_safe(0, "CMAKE_GENERATOR");
    if generator == "Ninja" {
        command += ", \"-f\", \"";
        command += makefile;
        command += "\"";
        command += &format!(", \"{target}\"");
    } else {
        command += &format!(", \"-f\", \"{makefile}\", \"{target}\"");
    }
    command
}

/// `cmExtraSublimeTextGenerator::AppendTarget`. The source-file flag map
/// upstream computes is never emitted, so it is not reproduced here.
fn append_target(
    s: &Shared,
    out: &mut String,
    target_name: &str,
    dir: usize,
    _target: Option<&Target>,
    make: &str,
    first_target: bool,
) {
    let makefile_name = if s.var_safe(0, "CMAKE_GENERATOR") == "Ninja" {
        "build.ninja"
    } else {
        "Makefile"
    };
    if !first_target {
        out.push_str(",\n\t");
    }
    out.push_str("\t{\n\t\t\t\"name\": \"");
    out.push_str(&s.project_name(dir));
    out.push_str(" - ");
    out.push_str(target_name);
    out.push_str("\",\n");
    out.push_str("\t\t\t\"cmd\": [");
    out.push_str(&build_make_command(s, make, makefile_name, target_name));
    out.push_str("],\n");
    out.push_str("\t\t\t\"working_dir\": \"${project_path}\",\n");
    out.push_str(
        "\t\t\t\"file_regex\": \"^(..[^:]*)(?::|\\\\()([0-9]+)(?::|\\\\))(?:([0-9]+):)?\\\\s*(.*)\"\n",
    );
    out.push_str("\t\t}");
}

/// `cmExtraSublimeTextGenerator::AppendAllTargets`.
fn append_all_targets(s: &Shared, out: &mut String, lgs: &[usize], make: &str) {
    if !lgs.is_empty() {
        append_target(s, out, "all", lgs[0], None, make, true);
        append_target(s, out, "clean", lgs[0], None, make, false);
    }
    for &dir in lgs {
        for target in s.targets(dir) {
            match target.kind {
                // Only the global targets from the top-level directory
                // (lg->GetCurrentBinaryDirectory() is the home output
                // directory).
                TargetKind::GlobalTarget if s.binary_dir(dir) == s.home_binary() => {
                    append_target(s, out, &target.name, dir, None, make, false);
                }
                TargetKind::GlobalTarget => {}
                TargetKind::Utility => {
                    let name = &target.name;
                    if (name.starts_with("Nightly") && name != "Nightly")
                        || (name.starts_with("Continuous") && name != "Continuous")
                        || (name.starts_with("Experimental") && name != "Experimental")
                    {
                        continue;
                    }
                    append_target(s, out, name, dir, None, make, false);
                }
                TargetKind::Executable
                | TargetKind::StaticLibrary
                | TargetKind::SharedLibrary
                | TargetKind::ModuleLibrary
                | TargetKind::ObjectLibrary => {
                    append_target(s, out, &target.name, dir, Some(target), make, false);
                    let fast = format!("{}/fast", target.name);
                    append_target(s, out, &fast, dir, Some(target), make, false);
                }
                _ => {}
            }
        }
        // The generator's global targets (listed after the directory's own
        // targets) get the same top-level-only filter.
        if s.binary_dir(dir) == s.home_binary() {
            for name in s.global_target_names(dir) {
                append_target(s, out, name, dir, None, make, false);
            }
        }
    }
}

/// `cmExtraSublimeTextGenerator::Generate` + `CreateNewProjectFile`.
pub fn generate(ctx: &GenCtx, globals: &[GlobalTarget]) {
    let s = Shared::new(ctx, globals);
    let exclude_build_folder = s
        .var(0, "CMAKE_SUBLIME_TEXT_2_EXCLUDE_BUILD_TREE")
        .is_some_and(|v| util::is_on(&v));
    let env_settings = s.var_safe(0, "CMAKE_SUBLIME_TEXT_2_ENV_SETTINGS");

    for (_, lgs) in s.project_map() {
        let dir = lgs[0];
        let output_dir = s.binary_dir(dir);
        let project_name = s.project_name(dir);
        let filename = format!("{output_dir}/{project_name}.sublime-project");

        let mut out = String::new();
        out.push_str("{\n\t\"folders\":\n\t[\n\t");
        // `lgs[0]->GetBinaryDirectory()` / `GetSourceDirectory()` are the
        // top-level directories, also for a sub-project's file.
        let source_root_relative = relative_path(s.home_binary(), s.home_source());
        if !source_root_relative.is_empty() {
            out.push_str("\t{\n\t\t\t\"path\": \"");
            out.push_str(&source_root_relative);
            out.push('"');
            let output_relative = relative_path(s.home_source(), s.home_binary());
            if !output_relative.is_empty()
                && (output_relative.len() < 3 || !output_relative.starts_with("../"))
                && exclude_build_folder
            {
                out.push_str(",\n\t\t\t\"folder_exclude_patterns\": [\"");
                out.push_str(&output_relative);
                out.push_str("\"]");
            }
        } else {
            out.push_str("\t{\n\t\t\t\"path\": \"./\"");
        }
        out.push_str("\n\t\t}");
        out.push_str("\n\t]");
        out.push_str(",\n\t\"build_systems\":\n\t[\n\t");

        let make = s.var_safe(dir, "CMAKE_MAKE_PROGRAM");
        append_all_targets(&s, &mut out, &lgs, &make);
        out.push_str("\n\t]");

        let system_name = s.var_safe(dir, "CMAKE_SYSTEM_NAME");
        if !env_settings.is_empty() {
            out.push(',');
            out.push_str("\n\t\"env\":\n\t{\n\t\t");
            out.push_str(&system_name);
            out.push_str(":\n\t\t{");
            for token in util::expand_list(&env_settings, false) {
                if let Some((var_name, var_value)) = token.split_once('=') {
                    out.push_str("\n\t\t\t\"");
                    out.push_str(var_name);
                    out.push_str("\":\"");
                    out.push_str(var_value);
                    out.push('"');
                }
            }
            out.push_str("\n\t\t}\n\t}");
        }
        out.push_str("\n}");
        write_file(&filename, &out);
    }
}
