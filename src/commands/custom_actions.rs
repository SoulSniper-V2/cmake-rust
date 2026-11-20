//! Generate-time custom-command actions (`cmMakefile::AddGeneratorAction`
//! with the `detail::` helpers of cmLocalGenerator.cxx).
//!
//! add_custom_command / add_custom_target validate and record their command
//! at configure time (`interp.ext.generator_actions`).  CMake attaches them to
//! source files and targets only when generation starts
//! (`cmMakefile::Generate` from `cmGlobalGenerator::Compute`), which is also
//! when "already has a custom rule", APPEND-lookup and output-path errors are
//! reported.  The generator calls [`run_generator_actions`] once, before it
//! computes anything.  Afterwards:
//!
//! - OUTPUT commands live on `ext.sources[dir][i].custom_command` (the
//!   MAIN_DEPENDENCY source or a `<output>.rule` source with `__CMAKE_RULE`).
//! - TARGET commands live on `Target::custom_commands` (kind PreBuild /
//!   PreLink / PostBuild, `target` set), in call order.
//! - A utility target's command lives on the `<bin>/CMakeFiles/<name>.rule`
//!   source; the target's SOURCES gains the SYMBOLIC
//!   `<bin>/CMakeFiles/<name>` (see [`utility_command`]).
//! - `ext.output_to_source[dir]` maps every non-genex output and byproduct to
//!   its producer (see [`source_with_output`]).

use crate::interp::Interp;
use crate::messages::{Frame, MessageType};
use crate::state::{CustomCommand, GeneratorAction, OutputSource};
use crate::util::{is_on, is_subdirectory};

use super::target::{find_known_source, find_source_index, has_genex, source_index};

/// `cmLocalGenerator::ExpandCustomCommandOutputGenex`: evaluates a genex
/// output path for every configuration and returns the unique full paths, or
/// an error message.  Supplied by the generator (genex engine).
pub type ExpandOutputGenex<'a> =
    dyn FnMut(&Interp, usize, &str, &[Frame]) -> Result<Vec<String>, String> + 'a;

/// The expander to use until the genex engine is integrated: genex outputs
/// are reported as unsupported.
#[allow(dead_code, reason = "generator integration API")]
pub fn unsupported_output_genex(
    _: &Interp,
    _: usize,
    output: &str,
    _: &[Frame],
) -> Result<Vec<String>, String> {
    Err(format!(
        "Error evaluating generator expression:\n  {output}\ngenerator expressions are not yet supported by this build of cmake-rust"
    ))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Primary,
    Byproduct,
}

impl Role {
    fn keyword(self) -> &'static str {
        match self {
            Role::Primary => "OUTPUT",
            Role::Byproduct => "BYPRODUCTS",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Origin {
    Project,
    Generator,
}

#[derive(Clone)]
enum Producer {
    Source(usize, Role),
    Target(String),
}

/// `cmMakefile::DoGenerate`'s action loop for every directory, in directory
/// order.  Diagnostics go through `interp.issue_at`; check `interp.error`
/// afterwards.
#[allow(dead_code, reason = "generator integration API")]
pub fn run_generator_actions(interp: &mut Interp, expand: &mut ExpandOutputGenex) {
    if interp.ext.generator_actions_invoked {
        return;
    }
    interp.ext.generator_actions_invoked = true;
    let mut actions = std::mem::take(&mut interp.ext.generator_actions);
    // Stable: keeps configure order within each directory.
    actions.sort_by_key(|action| action.command().makefile);
    for action in actions {
        match action {
            GeneratorAction::Output { command, replace } => {
                add_custom_command(interp, expand, command, replace);
            }
            GeneratorAction::Append(command) => append_custom_command(interp, expand, command),
            GeneratorAction::Target { target, command } => {
                add_target_command(interp, expand, &target, command);
            }
            GeneratorAction::Utility { target, command } => {
                add_utility_command(interp, expand, &target, command);
            }
        }
    }
}

/// `cmLocalGenerator::GetSourceFileWithOutput` / `GetSourcesWithOutput` for
/// full paths: the producer of `name` in directory `dir`.  Byproduct entries
/// are returned only with `include_byproducts`.
#[allow(dead_code, reason = "generator integration API")]
pub fn source_with_output<'a>(
    interp: &'a Interp,
    dir: usize,
    name: &str,
    include_byproducts: bool,
) -> Option<&'a OutputSource> {
    let entry = interp.ext.output_to_source.get(&dir)?.get(name)?;
    (include_byproducts || !entry.source_is_byproduct).then_some(entry)
}

/// The command of utility target `key` (attached to its `.rule` source by
/// `detail::AddUtilityCommand`), after [`run_generator_actions`].
#[allow(dead_code, reason = "generator integration API")]
pub fn utility_command<'a>(interp: &'a Interp, key: &str) -> Option<&'a CustomCommand> {
    let target = interp.ext.targets.get(key)?;
    let dir = target.makefile;
    let output = format!(
        "{}/CMakeFiles/{}",
        interp.makefiles[dir].binary_dir, target.name
    );
    let index = find_known_source(interp, dir, &format!("{output}.rule"))?;
    interp.ext.sources[&dir][index].custom_command.as_ref()
}

/// The directory's final value of a variable (normal binding, else cache).
fn final_def<'a>(interp: &'a Interp, dir: usize, name: &str) -> Option<&'a str> {
    match interp
        .dir_final_vars
        .get(dir)
        .and_then(|vars| vars.get(name))
    {
        Some(Some(value)) => Some(value),
        Some(None) => None,
        None => interp.cache.value(name),
    }
}

/// `cmMakefile::CanIWriteThisFile` at generate time.
fn can_write(interp: &Interp, dir: usize, path: &str) -> bool {
    if !final_def(interp, dir, "CMAKE_DISABLE_SOURCE_CHANGES").is_some_and(is_on) {
        return true;
    }
    let home = &interp.home_source;
    let output = &interp.home_binary;
    if same_file(home, output) {
        return !final_def(interp, dir, "CMAKE_DISABLE_IN_SOURCE_BUILD").is_some_and(is_on);
    }
    !is_subdirectory(path, home) || is_subdirectory(path, output) || same_file(path, output)
}

fn same_file(lhs: &str, rhs: &str) -> bool {
    match (std::fs::canonicalize(lhs), std::fs::canonicalize(rhs)) {
        (Ok(a), Ok(b)) => a == b,
        _ => lhs == rhs,
    }
}

/// `CreateGeneratedSource` (anonymous namespace of cmLocalGenerator.cxx).
fn create_generated_source(
    interp: &mut Interp,
    dir: usize,
    output: &str,
    role: Role,
    origin: Origin,
    backtrace: &[Frame],
) {
    if has_genex(output) {
        interp.issue_at(
            MessageType::FatalError,
            "Generator expressions in custom command outputs are not implemented!",
            backtrace,
        );
        return;
    }
    if !can_write(interp, dir, output) {
        interp.issue_at(
            MessageType::FatalError,
            &format!(
                "{} path\n  {output}\nin a source directory as an output of custom command.",
                role.keyword()
            ),
            backtrace,
        );
        return;
    }
    if let Some(bad) = output.chars().find(|c| matches!(c, '<' | '>')) {
        interp.issue_at(
            MessageType::FatalError,
            &format!("{} containing a \"{bad}\" is not allowed.", role.keyword()),
            backtrace,
        );
        return;
    }
    if origin == Origin::Generator {
        super::target::mark_generated(interp, dir, output);
    }
}

/// Both `cmLocalGenerator::UpdateOutputToSourceMap` overloads.
fn update_output_map(
    interp: &mut Interp,
    dir: usize,
    output: &str,
    producer: Producer,
    origin: Origin,
    backtrace: &[Frame],
) {
    let map = interp.ext.output_to_source.entry(dir).or_default();
    let role = match &producer {
        Producer::Source(_, role) => *role,
        Producer::Target(_) => Role::Byproduct,
    };
    if let Some(current) = map.get_mut(output) {
        match producer {
            Producer::Target(target) => {
                if current.target.is_none() {
                    current.target = Some(target);
                }
            }
            Producer::Source(index, role) => {
                if current.source.is_none()
                    || (current.source_is_byproduct && role == Role::Primary)
                {
                    current.source = Some(index);
                    current.source_is_byproduct = false;
                }
            }
        }
        return;
    }
    let entry = match producer {
        Producer::Source(index, role) => OutputSource {
            source: Some(index),
            source_is_byproduct: role == Role::Byproduct,
            target: None,
        },
        Producer::Target(target) => OutputSource {
            target: Some(target),
            ..Default::default()
        },
    };
    map.insert(output.to_owned(), entry);
    create_generated_source(interp, dir, output, role, origin, backtrace);
}

/// `AddSourceOutputs` / `AddTargetByproducts`.
fn add_outputs(
    interp: &mut Interp,
    expand: &mut ExpandOutputGenex,
    dir: usize,
    outputs: &[String],
    producer: &Producer,
    backtrace: &[Frame],
) {
    for output in outputs {
        if !has_genex(output) {
            update_output_map(
                interp,
                dir,
                output,
                producer.clone(),
                Origin::Project,
                backtrace,
            );
            continue;
        }
        match expand(interp, dir, output, backtrace) {
            Ok(paths) => {
                for path in paths {
                    update_output_map(
                        interp,
                        dir,
                        &path,
                        producer.clone(),
                        Origin::Generator,
                        backtrace,
                    );
                }
            }
            Err(message) => interp.issue_at(MessageType::FatalError, &message, backtrace),
        }
    }
}

/// `ComputeCustomCommandRuleFileName`.
fn rule_file_name(
    interp: &mut Interp,
    expand: &mut ExpandOutputGenex,
    dir: usize,
    output: &str,
    backtrace: &[Frame],
) -> Option<String> {
    if !has_genex(output) {
        return Some(output.to_owned());
    }
    let simple = output.replace("$<CONFIG>", "(CONFIG)");
    if !has_genex(&simple) {
        return Some(simple);
    }
    match expand(interp, dir, output, backtrace) {
        Ok(paths) if paths.len() == 1 => Some(paths.into_iter().next().unwrap()),
        Ok(_) => {
            let digest = <sha2::Sha256 as sha2::Digest>::digest(output.as_bytes());
            let hash: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
            Some(format!(
                "{}/CMakeFiles/{}",
                interp.makefiles[dir].binary_dir,
                &hash[..16]
            ))
        }
        Err(message) => {
            interp.issue_at(MessageType::FatalError, &message, backtrace);
            None
        }
    }
}

/// `AddCustomCommand` (anonymous namespace of cmLocalGenerator.cxx).
/// Returns the index of the source carrying the command.
fn add_custom_command(
    interp: &mut Interp,
    expand: &mut ExpandOutputGenex,
    mut command: CustomCommand,
    replace: bool,
) -> Option<usize> {
    let dir = command.makefile;
    let backtrace = command.backtrace.clone();
    let mut file = None;
    if !command.command_lines.is_empty() && !command.main_dependency.is_empty() {
        let main = command.main_dependency.clone();
        match find_source_index(interp, dir, &main) {
            Some(index) => {
                let existing = interp.ext.sources[&dir][index].custom_command.as_ref();
                match existing {
                    Some(existing) if !replace => {
                        if existing.command_lines == command.command_lines {
                            return Some(index);
                        }
                    }
                    _ => file = Some(index),
                }
            }
            None => file = Some(source_index(interp, dir, &main, false)),
        }
    }
    let index = match file {
        Some(index) => index,
        None => {
            let first = command.outputs.first()?.clone();
            let rule = format!(
                "{}.rule",
                rule_file_name(interp, expand, dir, &first, &backtrace)?
            );
            if let Some(index) = find_known_source(interp, dir, &rule)
                && let Some(existing) = &interp.ext.sources[&dir][index].custom_command
                && !replace
            {
                if existing.command_lines != command.command_lines {
                    interp.issue_at(
                        MessageType::FatalError,
                        &format!(
                            "Attempt to add a custom rule to output\n  {rule}\nwhich already has a custom rule."
                        ),
                        &backtrace,
                    );
                }
                return Some(index);
            }
            let index = source_index(interp, dir, &rule, true);
            interp.ext.sources.get_mut(&dir).unwrap()[index]
                .props
                .set("__CMAKE_RULE", "1".to_owned());
            index
        }
    };
    command.escape_allow_make_vars = true;
    let outputs = command.outputs.clone();
    let byproducts = command.byproducts.clone();
    add_outputs(
        interp,
        expand,
        dir,
        &outputs,
        &Producer::Source(index, Role::Primary),
        &backtrace,
    );
    add_outputs(
        interp,
        expand,
        dir,
        &byproducts,
        &Producer::Source(index, Role::Byproduct),
        &backtrace,
    );
    interp.ext.sources.get_mut(&dir).unwrap()[index].custom_command = Some(command);
    Some(index)
}

/// `detail::AppendCustomCommandToOutput`.
fn append_custom_command(
    interp: &mut Interp,
    expand: &mut ExpandOutputGenex,
    mut command: CustomCommand,
) {
    let dir = command.makefile;
    let backtrace = command.backtrace.clone();
    let output = command.outputs[0].clone();
    let lookup = |interp: &Interp, name: &str| {
        if crate::commands::target::full_path(name) {
            source_with_output(interp, dir, name, false).and_then(|entry| entry.source)
        } else {
            linear_source_with_output(interp, dir, name)
        }
    };
    let found = if has_genex(&output) {
        match expand(interp, dir, &output, &backtrace) {
            Ok(paths) => paths.iter().find_map(|path| lookup(interp, path)),
            Err(message) => {
                interp.issue_at(MessageType::FatalError, &message, &backtrace);
                return;
            }
        }
    } else {
        lookup(interp, &output)
    };
    if let Some(index) = found
        && let Some(existing) = interp.ext.sources.get_mut(&dir).unwrap()[index]
            .custom_command
            .as_mut()
    {
        existing.command_lines.append(&mut command.command_lines);
        existing.depends.append(&mut command.depends);
        let invalid = existing.codegen && !command.implicit_depends.is_empty();
        existing
            .implicit_depends
            .append(&mut command.implicit_depends);
        if invalid {
            interp.issue(
                MessageType::FatalError,
                "Cannot append IMPLICIT_DEPENDS to existing CODEGEN custom command.",
            );
        }
        return;
    }
    interp.issue_at(
        MessageType::FatalError,
        &format!(
            "Attempt to APPEND to custom command with output\n  {output}\nwhich is not already a custom command output."
        ),
        &backtrace,
    );
}

/// `AnyOutputMatches`.
fn any_output_matches(name: &str, outputs: &[String]) -> bool {
    outputs.iter().any(|output| {
        output.ends_with(name) && {
            let pos = output.len() - name.len();
            pos == 0 || output.as_bytes()[pos - 1] == b'/'
        }
    })
}

/// `cmLocalGenerator::LinearGetSourceFileWithOutput` (outputs only).
fn linear_source_with_output(interp: &Interp, dir: usize, name: &str) -> Option<usize> {
    interp.ext.sources.get(&dir)?.iter().position(|source| {
        source
            .custom_command
            .as_ref()
            .is_some_and(|command| any_output_matches(name, &command.outputs))
    })
}

/// `detail::AddCustomCommandToTarget`.
fn add_target_command(
    interp: &mut Interp,
    expand: &mut ExpandOutputGenex,
    key: &str,
    mut command: CustomCommand,
) {
    let dir = command.makefile;
    let backtrace = command.backtrace.clone();
    command.escape_allow_make_vars = true;
    command.target = Some(interp.ext.targets[key].name.clone());
    let byproducts = command.byproducts.clone();
    add_outputs(
        interp,
        expand,
        dir,
        &byproducts,
        &Producer::Target(key.to_owned()),
        &backtrace,
    );
    interp
        .ext
        .targets
        .get_mut(key)
        .unwrap()
        .custom_commands
        .push(command);
}

/// `detail::AddUtilityCommand` with `cmLocalGenerator::CreateUtilityOutput`.
fn add_utility_command(
    interp: &mut Interp,
    expand: &mut ExpandOutputGenex,
    key: &str,
    mut command: CustomCommand,
) {
    let dir = command.makefile;
    let backtrace = command.backtrace.clone();
    let byproducts = command.byproducts.clone();
    if command.comment.is_none() {
        command.comment = Some(String::new());
    }
    let name = interp.ext.targets[key].name.clone();
    let output = format!("{}/CMakeFiles/{name}", interp.makefiles[dir].binary_dir);
    super::target::mark_generated(interp, dir, &output);
    let index = source_index(interp, dir, &output, false);
    interp.ext.sources.get_mut(&dir).unwrap()[index]
        .props
        .set("SYMBOLIC", "1".to_owned());
    command.outputs = vec![output.clone()];
    if add_custom_command(interp, expand, command, false).is_some() {
        add_outputs(
            interp,
            expand,
            dir,
            &byproducts,
            &Producer::Target(key.to_owned()),
            &backtrace,
        );
    }
    let target = interp.ext.targets.get_mut(key).unwrap();
    target.insert_entry("SOURCES", &output, &backtrace, false);
}
