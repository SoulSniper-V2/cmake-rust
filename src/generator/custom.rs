//! cmCustomCommandGenerator. OWNER: generator-target worker.

#![allow(
    dead_code,
    reason = "custom command APIs are consumed by the Ninja writer integration"
)]

use std::path::Path;

use super::ctx::GenCtx;
use super::target::{
    ArtifactType, force_to_relative_path, full_path, source_prop, target_in_directory,
};
use crate::outconv::{FLAG_ALLOW_MAKE_VARIABLES, NINJA_UNIX, escape_for_shell};
use crate::state::TargetKind;
use crate::util::{
    collapse_full_path, expand_list, filename_name, filename_path, is_full_path, is_on_opt,
};

/// A custom command evaluated for one configuration.
#[derive(Clone, Debug)]
pub struct EvaluatedCommand {
    /// Each command line as argv (genex-evaluated, executable resolved for
    /// targets).
    pub command_lines: Vec<Vec<String>>,
    pub outputs: Vec<String>,
    pub byproducts: Vec<String>,
    pub depends: Vec<String>,
    pub working_directory: String,
    pub comment: Option<String>,
    /// Evaluated DEPFILE as configured, before CMP0116 transformation.
    pub depfile: String,
    /// Evaluated DEPFILE resolved against the command's binary directory.
    pub full_depfile: String,
    /// Target-level dependencies discovered from executable names and DEPENDS.
    /// INTEGRATION: also merge genex target references reported by the engine.
    pub target_dependencies: Vec<String>,
    pub job_pool: String,
    pub uses_terminal: bool,
    pub escape_old_style: bool,
    pub escape_allow_make_vars: bool,
}

/// Input for the generator, populated from the configured cmCustomCommand
/// model. Keep each command as argv and each list field as its unevaluated
/// arguments. Include MAIN_DEPENDENCY in `depends`, as cmCustomCommand does.
#[derive(Clone, Debug, Default)]
pub struct CustomCommandSpec {
    pub makefile: usize,
    /// Associated target of a TARGET command, or None for an OUTPUT command.
    pub target: Option<String>,
    pub command_lines: Vec<Vec<String>>,
    pub outputs: Vec<String>,
    pub byproducts: Vec<String>,
    pub depends: Vec<String>,
    pub working_directory: String,
    /// None means no COMMENT was provided. Some("") suppresses the default.
    pub comment: Option<String>,
    pub depfile: String,
    pub job_pool: String,
    pub uses_terminal: bool,
    pub verbatim: bool,
    pub escape_allow_make_vars: bool,
    pub command_expand_lists: bool,
    /// With CMP0116 NEW, the Ninja writer supplies its computed CMakeFiles/d
    /// path and uses that same path for the Ninja depfile binding.
    pub internal_depfile: Option<String>,
}

impl Default for EvaluatedCommand {
    fn default() -> Self {
        Self {
            command_lines: Vec::new(),
            outputs: Vec::new(),
            byproducts: Vec::new(),
            depends: Vec::new(),
            working_directory: String::new(),
            comment: None,
            depfile: String::new(),
            full_depfile: String::new(),
            target_dependencies: Vec::new(),
            job_pool: String::new(),
            uses_terminal: false,
            escape_old_style: true,
            escape_allow_make_vars: false,
        }
    }
}

/// Evaluate an empty command. Call `evaluate_spec` with a configured command.
pub fn evaluate(ctx: &GenCtx) -> EvaluatedCommand {
    evaluate_spec(ctx, &CustomCommandSpec::default())
}

/// Evaluate and resolve a custom command for single-config Ninja.
pub fn evaluate_spec(ctx: &GenCtx, spec: &CustomCommandSpec) -> EvaluatedCommand {
    let directory = &ctx.interp.makefiles[spec.makefile];
    let target = spec
        .target
        .as_deref()
        .and_then(|name| target_in_directory(ctx, spec.makefile, name));
    let cross_compiling = is_on_opt(ctx.var(spec.makefile, "CMAKE_CROSSCOMPILING").as_deref());
    let mut result = EvaluatedCommand {
        job_pool: spec.job_pool.clone(),
        uses_terminal: spec.uses_terminal,
        escape_old_style: !spec.verbatim,
        escape_allow_make_vars: spec.escape_allow_make_vars,
        ..EvaluatedCommand::default()
    };
    for line in &spec.command_lines {
        let mut argv = Vec::new();
        for argument in line {
            let value = evaluate_split_config(ctx, argument, target);
            if spec.command_expand_lists {
                argv.extend(expand_list(&value, false));
            } else {
                argv.push(value);
            }
        }
        if argv.is_empty() {
            argv.push(String::new());
        } else if let Some(executable) = target_in_directory(ctx, spec.makefile, &argv[0])
            .filter(|target| target.kind == TargetKind::Executable)
        {
            add_target_dependency(&mut result.target_dependencies, &executable.name);
            let emulator = executable.props.get("CROSSCOMPILING_EMULATOR");
            if executable.imported || !cross_compiling || emulator.is_some() {
                argv[0] = full_path(ctx, executable, &ctx.config, ArtifactType::Runtime, false);
            }
            if cross_compiling
                && !executable.imported
                && let Some(emulator) = emulator
            {
                let mut emulator = expand_list(&ctx.genex(emulator, None, None), false);
                if !emulator.is_empty() {
                    emulator.append(&mut argv);
                    argv = emulator;
                }
            }
        }
        result.command_lines.push(argv);
    }

    let output_paths = |paths: &[String]| {
        paths
            .iter()
            .flat_map(|path| expand_list(&ctx.genex(path, None, None), false))
            .map(|path| unix_full_path(&path, &directory.binary_dir))
            .collect()
    };
    result.outputs = output_paths(&spec.outputs);
    result.byproducts = output_paths(&spec.byproducts);

    for dependency in &spec.depends {
        let value = evaluate_split_config(ctx, dependency, None);
        for dependency in expand_list(&value, false) {
            let dependency = if is_full_path(&dependency) {
                unix_full_path(&dependency, "")
            } else {
                dependency.replace('\\', "/")
            };
            let (path, target) = resolve_dependency(ctx, spec.makefile, &dependency);
            if let Some(path) = path {
                result.depends.push(path);
            }
            if let Some(target) = target {
                add_target_dependency(&mut result.target_dependencies, target);
            }
        }
    }

    let working_directory = evaluate_split_config(ctx, &spec.working_directory, target);
    if !working_directory.is_empty() {
        result.working_directory = unix_full_path(&working_directory, &directory.binary_dir);
    }
    result.comment = spec
        .comment
        .as_ref()
        .map(|comment| ctx.genex(comment, None, None));
    result.depfile = ctx.genex(&spec.depfile, None, None);
    if !result.depfile.is_empty() {
        result.full_depfile = unix_full_path(&result.depfile, &directory.binary_dir);
    }
    if let Some(transformed_path) = &spec.internal_depfile
        && !spec.depfile.is_empty()
        && !result.command_lines.is_empty()
    {
        result.command_lines.push(vec![
            ctx.interp.cmake_command.clone(),
            "-E".to_owned(),
            "cmake_transform_depfile".to_owned(),
            "Ninja".to_owned(),
            "gccdepfile".to_owned(),
            ctx.interp.home_source.clone(),
            directory.source_dir.clone(),
            ctx.interp.home_binary.clone(),
            directory.binary_dir.clone(),
            result.full_depfile.clone(),
            transformed_path.clone(),
        ]);
    }
    result
}

fn add_target_dependency(deps: &mut Vec<String>, name: &str) {
    if !deps.iter().any(|dep| dep == name) {
        deps.push(name.to_owned());
    }
}

fn unix_full_path(path: &str, base: &str) -> String {
    collapse_full_path(&path.replace('\\', "/"), base)
}

fn evaluate_split_config(
    ctx: &GenCtx,
    text: &str,
    target: Option<&crate::state::Target>,
) -> String {
    let mut result = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("$<") {
        result.push_str(&rest[..start]);
        let input = &rest[start..];
        let mut depth = 0;
        let mut end = None;
        for (index, byte) in input.bytes().enumerate() {
            if byte == b'<' && index > 0 && input.as_bytes()[index - 1] == b'$' {
                depth += 1;
            } else if byte == b'>' {
                depth -= 1;
                if depth == 0 {
                    end = Some(index + 1);
                    break;
                }
            }
        }
        let len = end.unwrap_or(input.len());
        let expression = &input[..len];
        let to_evaluate = if end.is_some() {
            expression
                .strip_prefix("$<COMMAND_CONFIG:")
                .or_else(|| expression.strip_prefix("$<OUTPUT_CONFIG:"))
                .and_then(|expression| expression.strip_suffix('>'))
                .unwrap_or(expression)
        } else {
            expression
        };
        // For single-config Ninja both wrappers use the same configuration.
        result.push_str(&ctx.genex(to_evaluate, target, None));
        rest = &input[len..];
    }
    result.push_str(rest);
    result
}

/// `cmLocalGenerator::GetRealDependency`. A target-only dependency has no path.
pub fn real_dependency(ctx: &GenCtx, makefile: usize, dependency: &str) -> Option<String> {
    resolve_dependency(ctx, makefile, dependency).0
}

fn resolve_dependency<'a>(
    ctx: &GenCtx<'a>,
    makefile: usize,
    dependency: &str,
) -> (Option<String>, Option<&'a str>) {
    let name = filename_name(dependency);
    if name.is_empty() {
        return (None, None);
    }
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    let absolute = is_full_path(dependency);
    if let Some(target) = target_in_directory(ctx, makefile, name) {
        let has_location = matches!(
            target.kind,
            TargetKind::Executable
                | TargetKind::StaticLibrary
                | TargetKind::SharedLibrary
                | TargetKind::ModuleLibrary
        );
        if absolute {
            let target_directory = has_location.then(|| {
                let path = full_path(ctx, target, &ctx.config, ArtifactType::Runtime, false);
                collapse_full_path(&filename_path(&path), "")
            });
            let dependency_directory = collapse_full_path(&filename_path(dependency), "");
            if target_directory.as_deref() != Some(dependency_directory.as_str()) {
                return (Some(dependency.to_owned()), None);
            }
        }
        let path = (has_location || target.kind == TargetKind::UnknownLibrary)
            .then(|| full_path(ctx, target, &ctx.config, ArtifactType::Runtime, true));
        return (path, Some(&target.name));
    }
    if absolute {
        return (Some(dependency.to_owned()), None);
    }

    let directory = &ctx.interp.makefiles[makefile];
    let source = collapse_full_path(dependency, &directory.source_dir);
    let binary = collapse_full_path(dependency, &directory.binary_dir);
    // INTEGRATION: resolve known dependency sources through the SourceFile
    // registry before falling back to a path under the source or binary tree.
    let generated =
        |path: &str| is_on_opt(source_prop(ctx, makefile, path, "GENERATED").as_deref());
    let path = if generated(dependency) || generated(&binary) {
        binary
    } else if generated(&source) || Path::new(&source).exists() {
        source
    } else {
        binary
    };
    (Some(path), None)
}

fn path_eq_or_subdir(a: &str, b: &str) -> bool {
    a == b || crate::util::is_subdirectory(a, b)
}

/// `cmOutputConverter::MaybeRelativeTo(local, remote)` for the output
/// converter of directory `makefile` (relative-path tops computed as in
/// ComputeRelativePathTopSource/Binary).
pub fn maybe_relative_to(ctx: &GenCtx, makefile: usize, local: &str, remote: &str) -> String {
    let makefiles = &ctx.interp.makefiles;
    let top = |pick: fn(&crate::interp::Makefile) -> &str| {
        let mut current = makefile;
        let mut parent = makefiles[makefile].parent;
        while let Some(index) = parent {
            if crate::util::is_subdirectory(pick(&makefiles[current]), pick(&makefiles[index])) {
                current = index;
            }
            parent = makefiles[index].parent;
        }
        pick(&makefiles[current]).to_owned()
    };
    let top_source = top(|m| &m.source_dir);
    let top_binary = top(|m| &m.binary_dir);
    let mut local_in_binary = path_eq_or_subdir(local, &top_binary);
    let mut remote_in_binary = path_eq_or_subdir(remote, &top_binary);
    let mut local_in_source = path_eq_or_subdir(local, &top_source);
    let mut remote_in_source = path_eq_or_subdir(remote, &top_source);
    if top_source == top_binary {
        // TopRelation::InSource: checks are identical.
    } else if crate::util::is_subdirectory(&top_binary, &top_source) {
        local_in_source &= !local_in_binary;
        remote_in_source &= !remote_in_binary;
    } else if crate::util::is_subdirectory(&top_source, &top_binary) {
        local_in_binary &= !local_in_source;
        remote_in_binary &= !remote_in_source;
    }
    if (local_in_binary && remote_in_binary) || (local_in_source && remote_in_source) {
        force_to_relative_path(local, remote)
    } else {
        remote.to_owned()
    }
}

impl EvaluatedCommand {
    /// COMMENT, or the same default description used by the Ninja generator.
    pub fn description(&self, ctx: &GenCtx, makefile: usize, fallback: &str) -> String {
        if let Some(comment) = &self.comment {
            return comment.clone();
        }
        if self.outputs.is_empty() {
            return fallback.to_owned();
        }
        let directory = &ctx.interp.makefiles[makefile].binary_dir;
        let paths = self
            .outputs
            .iter()
            .map(|output| maybe_relative_to(ctx, makefile, directory, output))
            .collect::<Vec<_>>();
        format!("Generating {}", paths.join(", "))
    }

    /// AppendArguments. The result is ready for a Ninja variable value.
    pub fn append_arguments(&self, line: usize, command: &mut String) {
        let flags = NINJA_UNIX
            | if self.escape_allow_make_vars {
                FLAG_ALLOW_MAKE_VARIABLES
            } else {
                0
            };
        for argument in self.command_lines[line].iter().skip(1) {
            command.push(' ');
            if self.escape_old_style {
                command.push_str(&argument.replace(' ', "\\ "));
            } else {
                command.push_str(&escape_for_shell(argument, flags));
            }
        }
    }

    /// cmLocalNinjaGenerator::AppendCustomCommandLines, before chain joining
    /// or adding a RULE_LAUNCH_CUSTOM launcher.
    pub fn shell_lines(&self, ctx: &GenCtx, makefile: usize) -> Vec<String> {
        if self.command_lines.is_empty() {
            return Vec::new();
        }
        let flags = NINJA_UNIX | FLAG_ALLOW_MAKE_VARIABLES;
        let directory = if self.working_directory.is_empty() {
            &ctx.interp.makefiles[makefile].binary_dir
        } else {
            &self.working_directory
        };
        let mut lines = vec![format!("cd {}", escape_for_shell(directory, flags))];
        for (index, argv) in self.command_lines.iter().enumerate() {
            if argv[0].is_empty() {
                continue;
            }
            let mut line = escape_for_shell(&argv[0], flags);
            self.append_arguments(index, &mut line);
            lines.push(line);
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generator::target::tests::{fixture_interp, oracle_fixture, test_target};

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn oracle_custom_command_and_verbatim_escaping() {
        let interp = fixture_interp("/project");
        let ctx = GenCtx::new(&interp);
        let spec = CustomCommandSpec {
            makefile: 1,
            command_lines: vec![args(&[
                "/opt/cmake-oracle/bin/cmake",
                "-E",
                "touch",
                "gen/generated.c",
                "a b",
                "$literal",
            ])],
            outputs: args(&["gen/generated.c"]),
            byproducts: args(&["gen/stamp"]),
            verbatim: true,
            ..CustomCommandSpec::default()
        };
        let evaluated = evaluate_spec(&ctx, &spec);
        let fixture = oracle_fixture();
        let expected = &fixture["custom"];
        let outputs: Vec<_> = evaluated
            .outputs
            .iter()
            .chain(&evaluated.byproducts)
            .map(|path| path.strip_prefix("/project/build/").unwrap())
            .collect();
        assert_eq!(
            outputs,
            expected["outputs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|path| path.as_str().unwrap())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            evaluated.description(&ctx, 1, ""),
            expected["description"].as_str().unwrap()
        );
        assert_eq!(
            evaluated
                .shell_lines(&ctx, 1)
                .join(" && ")
                .replace("/project/", "")
                .replace("/opt/cmake-oracle/bin/cmake", "<cmake>"),
            expected["command"].as_str().unwrap()
        );
        assert!(ctx.errors.borrow().is_empty());
        assert!(evaluate(&ctx).shell_lines(&ctx, 0).is_empty());
    }

    #[test]
    fn target_commands_dependencies_and_depfile() {
        let mut interp = fixture_interp("/project");
        interp.dir_final_vars[1].insert("CMAKE_CROSSCOMPILING".to_owned(), Some("ON".to_owned()));
        interp.cmake_command = "/cmake".to_owned();
        let tool = test_target(
            "tool",
            TargetKind::Executable,
            1,
            &[
                ("LINKER_LANGUAGE", "C"),
                ("VERSION", "2"),
                (
                    "CROSSCOMPILING_EMULATOR",
                    "/bin/emulator;argument with spaces",
                ),
            ],
        );
        interp.ext.targets.insert(tool.name.clone(), tool);
        let utility = test_target("prerequisite", TargetKind::Utility, 1, &[]);
        interp.ext.targets.insert(utility.name.clone(), utility);
        let mut external = test_target(
            "external",
            TargetKind::Executable,
            0,
            &[
                ("IMPORTED_LOCATION", "/usr/bin/external"),
                ("CROSSCOMPILING_EMULATOR", "/bin/unneeded"),
            ],
        );
        external.imported = true;
        interp.ext.targets.insert(external.name.clone(), external);

        let ctx = GenCtx::new(&interp);
        let spec = CustomCommandSpec {
            makefile: 1,
            command_lines: vec![
                args(&["tool", "$<COMMAND_CONFIG:first;second>", ""]),
                args(&["external", "$<OUTPUT_CONFIG:literal>"]),
            ],
            depends: args(&["tool.exe", "prerequisite", "/unrelated/tool", "such/file"]),
            command_expand_lists: true,
            working_directory: "../work/./here".to_owned(),
            comment: Some(String::new()),
            depfile: "path/../file.d".to_owned(),
            internal_depfile: Some("/project/build/CMakeFiles/d/id.d".to_owned()),
            job_pool: "lto".to_owned(),
            uses_terminal: true,
            ..CustomCommandSpec::default()
        };
        let evaluated = evaluate_spec(&ctx, &spec);
        assert_eq!(
            evaluated.command_lines[0],
            args(&[
                "/bin/emulator",
                "argument with spaces",
                "/project/build/nested/oldbin/tool",
                "first",
                "second"
            ])
        );
        assert_eq!(
            evaluated.command_lines[1],
            args(&["/usr/bin/external", "literal"])
        );
        assert_eq!(
            evaluated.target_dependencies,
            args(&["tool", "external", "prerequisite"])
        );
        assert_eq!(
            evaluated.depends,
            args(&[
                "/project/build/nested/oldbin/tool-2",
                "/unrelated/tool",
                "/project/build/nested/such/file"
            ])
        );
        assert_eq!(evaluated.working_directory, "/project/build/work/here");
        assert_eq!(evaluated.depfile, "path/../file.d");
        assert_eq!(evaluated.full_depfile, "/project/build/nested/file.d");
        assert_eq!(
            &evaluated.command_lines[2],
            &args(&[
                "/cmake",
                "-E",
                "cmake_transform_depfile",
                "Ninja",
                "gccdepfile",
                "/project/src",
                "/project/src/nested",
                "/project/build",
                "/project/build/nested",
                "/project/build/nested/file.d",
                "/project/build/CMakeFiles/d/id.d",
            ])
        );
        assert_eq!(evaluated.description(&ctx, 1, "fallback"), "");
        assert_eq!(evaluated.job_pool, "lto");
        assert!(evaluated.uses_terminal);
        assert_eq!(
            evaluated.shell_lines(&ctx, 1)[1],
            r"/bin/emulator argument\ with\ spaces /project/build/nested/oldbin/tool first second"
        );
        assert_eq!(real_dependency(&ctx, 1, "prerequisite"), None);
        assert!(ctx.errors.borrow().is_empty());
    }
}
