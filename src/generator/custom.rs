//! cmCustomCommandGenerator. OWNER: generator-target worker.

#![allow(
    dead_code,
    reason = "custom command APIs are consumed by the Ninja writer integration"
)]

use std::path::Path;

use super::ctx::GenCtx;
use super::target::{ArtifactType, force_to_relative_path, full_path, target_in_directory};
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
    /// `cmLocalNinjaGenerator::AppendCustomCommandDeps`: the real (file)
    /// dependency of each evaluated DEPENDS entry (`GetRealDependency`).
    pub depends: Vec<String>,
    /// `cmCustomCommandGenerator::GetDepends`: evaluated DEPENDS entries.
    pub raw_depends: Vec<String>,
    /// `cmCustomCommandGenerator::GetUtilities`: (target, cross) referenced
    /// by command lines (argv0 executables and genex targets).
    pub utilities: Vec<(String, bool)>,
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
    pub job_server_aware: bool,
    pub uses_terminal: bool,
    pub codegen: bool,
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
    pub job_server_aware: bool,
    pub uses_terminal: bool,
    pub codegen: bool,
    pub verbatim: bool,
    pub escape_allow_make_vars: bool,
    pub command_expand_lists: bool,
    /// `transformDepfile` (CMP0116 NEW): append the
    /// `cmake_transform_depfile` step writing `internal_depfile_name`.
    pub transform_depfile: bool,
    /// The command's backtrace (diagnostics).
    pub backtrace: Vec<crate::messages::Frame>,
}

impl Default for EvaluatedCommand {
    fn default() -> Self {
        Self {
            command_lines: Vec::new(),
            outputs: Vec::new(),
            byproducts: Vec::new(),
            depends: Vec::new(),
            raw_depends: Vec::new(),
            utilities: Vec::new(),
            working_directory: String::new(),
            comment: None,
            depfile: String::new(),
            full_depfile: String::new(),
            target_dependencies: Vec::new(),
            job_pool: String::new(),
            job_server_aware: false,
            uses_terminal: false,
            codegen: false,
            escape_old_style: true,
            escape_allow_make_vars: false,
        }
    }
}

/// Evaluate an empty command. Call `evaluate_spec` with a configured command.
pub fn evaluate(ctx: &GenCtx) -> EvaluatedCommand {
    evaluate_spec(ctx, &CustomCommandSpec::default())
}

/// Evaluate and resolve a custom command for single-config Ninja
/// (`cmCustomCommandGenerator` constructor + accessors).
pub fn evaluate_spec(ctx: &GenCtx, spec: &CustomCommandSpec) -> EvaluatedCommand {
    evaluate_spec_cfg(ctx, spec, &ctx.config.clone(), &ctx.config.clone())
}

/// `cmCustomCommandGenerator(cc, commandConfig, lg, outputConfig)`:
/// `output_config` is the configuration the evaluated outputs belong to,
/// `command_config` the configuration of the file holding the command.
pub fn evaluate_spec_cfg(
    ctx: &GenCtx,
    spec: &CustomCommandSpec,
    output_config: &str,
    command_config: &str,
) -> EvaluatedCommand {
    let dir = spec.makefile;
    let directory = &ctx.interp.makefiles[dir];
    let target = spec
        .target
        .as_deref()
        .and_then(|name| target_in_directory(ctx, dir, name));
    let target_key = target.map(|target| ctx.target_key(target));
    let cross_compiling = is_on_opt(ctx.var(dir, "CMAKE_CROSSCOMPILING").as_deref());
    let mut result = EvaluatedCommand {
        job_pool: spec.job_pool.clone(),
        job_server_aware: spec.job_server_aware,
        uses_terminal: spec.uses_terminal,
        codegen: spec.codegen,
        escape_old_style: !spec.verbatim,
        escape_allow_make_vars: spec.escape_allow_make_vars,
        ..EvaluatedCommand::default()
    };
    let cmd_ctx = ctx.for_config(command_config);
    let distinct = output_config != command_config;
    let mut utilities = Vec::new();
    for line in &spec.command_lines {
        let mut argv = Vec::new();
        let mut use_output_config = false;
        for argument in line {
            let value = ctx.genex_command(
                argument,
                dir,
                target_key.as_deref(),
                use_output_config,
                output_config,
                command_config,
                &spec.backtrace,
                Some(&mut utilities),
            );
            if distinct {
                use_output_config = true;
            }
            if spec.command_expand_lists {
                argv.extend(expand_list(&value, false));
            } else {
                argv.push(value);
            }
        }
        if argv.is_empty() {
            argv.push(String::new());
        } else if let Some(executable) = target_in_directory(ctx, dir, &argv[0])
            .filter(|target| target.kind == TargetKind::Executable)
        {
            if !utilities
                .iter()
                .any(|(name, cross)| *name == executable.name && *cross)
            {
                utilities.push((executable.name.clone(), true));
            }
            add_target_dependency(&mut result.target_dependencies, &executable.name);
            // GetArgv0Location / emulator handling.
            let emulator = executable.props.get("CROSSCOMPILING_EMULATOR");
            if executable.imported || !cross_compiling || emulator.is_some() {
                argv[0] = full_path(
                    ctx,
                    executable,
                    command_config,
                    ArtifactType::Runtime,
                    false,
                );
            }
            if cross_compiling
                && !executable.imported
                && let Some(emulator) = emulator
            {
                let mut emulator = expand_list(&cmd_ctx.genex(emulator, None, None), false);
                if !emulator.is_empty() {
                    emulator.append(&mut argv);
                    argv = emulator;
                }
            }
        }
        result.command_lines.push(argv);
    }
    for (name, _) in &utilities {
        add_target_dependency(&mut result.target_dependencies, name);
    }
    result.utilities = utilities;

    // EvaluateOutputs: ExpandCustomCommandOutputPaths.
    let output_paths = |paths: &[String]| {
        paths
            .iter()
            .flat_map(|path| {
                expand_list(
                    &ctx.genex_command(
                        path,
                        dir,
                        None,
                        true,
                        output_config,
                        command_config,
                        &spec.backtrace,
                        None,
                    ),
                    false,
                )
            })
            .map(|path| collapse_full_path(&path, &directory.binary_dir))
            .collect()
    };
    result.outputs = output_paths(&spec.outputs);
    result.byproducts = output_paths(&spec.byproducts);

    // EvaluateDepends.
    for dependency in &spec.depends {
        let value = ctx.genex_command(
            dependency,
            dir,
            None,
            true,
            output_config,
            command_config,
            &spec.backtrace,
            None,
        );
        for dependency in expand_list(&value, false) {
            let dependency = if is_full_path(&dependency) {
                collapse_full_path(&dependency, "")
            } else {
                dependency.replace('\\', "/")
            };
            result.raw_depends.push(dependency);
        }
    }
    for dependency in &result.raw_depends {
        let (path, target) = resolve_dependency(&cmd_ctx, dir, dependency);
        if let Some(path) = path {
            result.depends.push(path);
        }
        if let Some(target) = target {
            add_target_dependency(&mut result.target_dependencies, target);
        }
    }

    let working_directory = ctx.genex_command(
        &spec.working_directory,
        dir,
        target_key.as_deref(),
        true,
        output_config,
        command_config,
        &spec.backtrace,
        None,
    );
    if !working_directory.is_empty() {
        result.working_directory = unix_full_path(&working_directory, &directory.binary_dir);
    }
    result.comment = spec.comment.as_ref().map(|comment| {
        ctx.genex_command(
            comment,
            dir,
            None,
            true,
            output_config,
            command_config,
            &spec.backtrace,
            None,
        )
    });
    result.depfile = ctx.genex_command(
        &spec.depfile,
        dir,
        None,
        true,
        output_config,
        command_config,
        &spec.backtrace,
        None,
    );
    if !result.depfile.is_empty() {
        result.full_depfile = unix_full_path(&result.depfile, &directory.binary_dir);
    }
    if spec.transform_depfile && !spec.depfile.is_empty() && !result.command_lines.is_empty() {
        result.command_lines.push(vec![
            ctx.interp.cmake_command.clone(),
            "-E".to_owned(),
            "cmake_transform_depfile".to_owned(),
            if ctx.multi_config {
                "Ninja Multi-Config"
            } else {
                "Ninja"
            }
            .to_owned(),
            "gccdepfile".to_owned(),
            ctx.interp.home_source.clone(),
            directory.source_dir.clone(),
            ctx.interp.home_binary.clone(),
            directory.binary_dir.clone(),
            result.full_depfile.clone(),
            internal_depfile_name(ctx, &result.full_depfile),
        ]);
    }
    result
}

/// `cmCustomCommandGenerator::GetInternalDepfileName` (gcc depfile format).
pub fn internal_depfile_name(ctx: &GenCtx, full_depfile: &str) -> String {
    let digest = <sha2::Sha256 as sha2::Digest>::digest(full_depfile.as_bytes());
    let hash: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{}/CMakeFiles/d/{hash}.d", ctx.interp.home_binary)
}

/// `cmLocalNinjaGenerator::MakeCustomCommandGenerators`: the output
/// configuration and evaluated command of the file's configuration, plus the
/// other cross configurations when every output and byproduct is a generator
/// expression with values unique to each configuration.
pub fn make_generators(
    ctx: &GenCtx,
    command: &crate::state::CustomCommand,
    file_config: &str,
) -> Vec<(String, EvaluatedCommand)> {
    let spec = spec_from_command(ctx, command);
    let mut ccgs = vec![(
        file_config.to_owned(),
        evaluate_spec_cfg(ctx, &spec, file_config, file_config),
    )];
    if !ctx.multi_config
        || ctx.cross_configs.is_empty()
        || !command
            .outputs
            .iter()
            .chain(&command.byproducts)
            .all(|output| output.contains("$<"))
    {
        return ccgs;
    }
    // GetCrossConfigs(fileConfig): a sorted std::set.
    for config in &ctx.cross_configs {
        if config != file_config {
            ccgs.push((
                config.clone(),
                evaluate_spec_cfg(ctx, &spec, config, file_config),
            ));
        }
    }
    // HasUniqueOutputs.
    let mut outputs = std::collections::HashSet::new();
    let mut byproducts = std::collections::HashSet::new();
    let unique = ccgs.iter().all(|(_, evaluated)| {
        evaluated
            .outputs
            .iter()
            .all(|output| outputs.insert(output.clone()))
            && evaluated
                .byproducts
                .iter()
                .all(|byproduct| byproducts.insert(byproduct.clone()))
    });
    if !unique {
        ccgs.truncate(1);
    }
    ccgs
}

/// A configured custom command as the generator's input.
pub fn spec_from_command(ctx: &GenCtx, command: &crate::state::CustomCommand) -> CustomCommandSpec {
    let mut spec = CustomCommandSpec {
        makefile: command.makefile,
        target: command.target.clone(),
        command_lines: command.command_lines.clone(),
        outputs: command.outputs.clone(),
        byproducts: command.byproducts.clone(),
        depends: command.depends.clone(),
        working_directory: command.working_directory.clone(),
        comment: command.comment.clone(),
        depfile: command.depfile.clone(),
        job_pool: command.job_pool.clone(),
        job_server_aware: command.job_server_aware,
        uses_terminal: command.uses_terminal,
        codegen: command.codegen,
        verbatim: command.verbatim,
        escape_allow_make_vars: command.escape_allow_make_vars,
        command_expand_lists: command.command_expand_lists,
        transform_depfile: false,
        backtrace: command.backtrace.clone(),
    };
    // MakeCustomCommandGenerators: transformDepfile with CMP0116 NEW. The
    // internal depfile path is computed per configuration by evaluate_spec.
    let cmp0116 = crate::policy::index_of("CMP0116").and_then(|index| command.policies.get(index));
    if cmp0116 == Some(crate::policy::PolicyStatus::New)
        && !command.depfile.is_empty()
        && ctx.interp.get_def("CMAKE_GENERATOR") != Some("Unix Makefiles")
    {
        spec.transform_depfile = true;
    }
    spec
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
    // cmMakefile::GetSource + cmSourceFile::ResolveFullPath.
    let source = collapse_full_path(dependency, &directory.source_dir);
    let binary = collapse_full_path(dependency, &directory.binary_dir);
    if let Some(index) =
        crate::commands::target::find_source_index(ctx.interp, makefile, dependency)
    {
        let sf = &ctx.interp.ext.sources[&makefile][index];
        let location = &sf.location;
        let lpath = location.path();
        let generated = sf.generated || is_on_opt(sf.props.get("GENERATED"));
        if generated {
            // cmSourceFile::FindFullPath: generated files are located in the
            // binary directory without checking the disk.
            let mut resolved = location.clone();
            if resolved.ambiguous_directory {
                resolved.directory = collapse_full_path(&resolved.directory, &directory.binary_dir);
                resolved.ambiguous_directory = false;
            }
            return (Some(resolved.path()), None);
        }
        // cmSourceFile::FindFullPath: non-generated files are sought in the
        // source directory and then in the binary directory for ambiguous
        // relative locations.  Crucially, resolve the source object matched
        // by this dependency rather than independently preferring a generated
        // output that merely has the same relative name.
        let directories: &[&str] = if location.ambiguous_directory {
            &[directory.source_dir.as_str(), directory.binary_dir.as_str()]
        } else {
            &[""]
        };
        for dir in directories {
            let full = collapse_full_path(&lpath, dir);
            if ctx.interp.ext.generated_sources.contains(&full) || Path::new(&full).exists() {
                return (Some(full), None);
            }
        }
    }
    // Relative to the source directory in which it was given, else assume it
    // lives in the binary directory.
    if Path::new(&source).exists() {
        (Some(source), None)
    } else {
        (Some(binary), None)
    }
}

fn path_eq_or_subdir(a: &str, b: &str) -> bool {
    a == b || crate::util::is_subdirectory(a, b)
}

/// `cmOutputConverter::MaybeRelativeTo(local, remote)` for the output
/// converter of directory `makefile` (relative-path tops computed as in
/// ComputeRelativePathTopSource/Binary).
/// cmOutputConverter::ComputeRelativePathTopSource/TopBinary: the highest
/// buildsystem ancestor directory that still contains this directory's
/// source (binary) directory. Returns (top source, top binary).
pub fn relative_path_tops(ctx: &GenCtx, makefile: usize) -> (String, String) {
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
    (top(|m| &m.source_dir), top(|m| &m.binary_dir))
}

pub fn maybe_relative_to(ctx: &GenCtx, makefile: usize, local: &str, remote: &str) -> String {
    let (top_source, top_binary) = relative_path_tops(ctx, makefile);
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
    pub fn append_arguments(&self, ctx: &GenCtx, line: usize, command: &mut String) {
        let flags = NINJA_UNIX
            | if self.escape_allow_make_vars {
                FLAG_ALLOW_MAKE_VARIABLES
            } else {
                0
            }
            | if self.escape_allow_make_vars && ctx.multi_config {
                // MakeVars && IsNinjaMulti()
                crate::outconv::FLAG_UNESCAPE_NINJA_CONFIGURATION
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
        let flags = NINJA_UNIX
            | FLAG_ALLOW_MAKE_VARIABLES
            | if ctx.multi_config {
                crate::outconv::FLAG_UNESCAPE_NINJA_CONFIGURATION
            } else {
                0
            };
        let directory = if self.working_directory.is_empty() {
            &ctx.interp.makefiles[makefile].binary_dir
        } else {
            &self.working_directory
        };
        let mut lines = vec![format!(
            "cd {}",
            escape_for_shell(directory, NINJA_UNIX | FLAG_ALLOW_MAKE_VARIABLES)
        )];
        for (index, argv) in self.command_lines.iter().enumerate() {
            if argv[0].is_empty() {
                continue;
            }
            let mut line = escape_for_shell(&argv[0], flags);
            self.append_arguments(ctx, index, &mut line);
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
        // Directory-scoped in the top directory, inherited by directory 1.
        for dir in [0, 1] {
            interp
                .ext
                .scoped_targets
                .insert((dir, external.name.clone()), external.name.clone());
        }
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
            transform_depfile: true,
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
                &internal_depfile_name(&ctx, "/project/build/nested/file.d"),
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
