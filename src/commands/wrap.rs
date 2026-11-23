//! The legacy source wrapping commands `fltk_wrap_ui`, `qt_wrap_cpp` and
//! `qt_wrap_ui`, and `include_external_msproject`.

use super::target::{mark_generated, mark_generated_ambiguous};
use super::targets;
use crate::interp::{Builtin, CmdResult, Interp, Mode, err};
use crate::messages::{Frame, MessageType};
use crate::state::{CustomCommand, GeneratorAction, find_source, get_or_create_source};
use crate::util::{
    collapse_full_path, filename_name, filename_without_last_extension, is_full_path, is_on,
};

pub fn register(interp: &mut Interp) {
    if interp.mode != Mode::Project {
        return;
    }
    let commands: [(&str, Builtin); 4] = [
        ("fltk_wrap_ui", fltk_wrap_ui),
        ("qt_wrap_cpp", qt_wrap_cpp),
        ("qt_wrap_ui", qt_wrap_ui),
        ("include_external_msproject", include_external_msproject),
    ];
    for (name, command) in commands {
        interp.register(name, command);
    }
}

/// `cmMakefile::GetRequiredDefinition`.
fn required_definition(interp: &mut Interp, name: &str) -> String {
    if let Some(value) = interp.get_def(name) {
        return value.to_owned();
    }
    interp.error = true;
    interp.stderr_line(&format!(
        "CMake Error: Error required internal CMake variable not set, cmake may not be built correctly.\nMissing variable is:\n{name}"
    ));
    String::new()
}

/// A source's `(WRAP_EXCLUDE, ABSTRACT, generated)`, if it is known.
fn known_source(interp: &Interp, name: &str) -> Option<(bool, Option<String>, bool)> {
    let source = find_source(interp, interp.mf_index(), name)?;
    Some((
        source.props.get("WRAP_EXCLUDE").is_some_and(is_on),
        source.props.get("ABSTRACT").map(str::to_owned),
        source.generated,
    ))
}

/// `cmMakefile::AddCustomCommandToOutput` with one command line.
fn add_output(
    interp: &mut Interp,
    output: &str,
    depends: &[&str],
    line: &[&str],
    comment: Option<&str>,
) {
    let owned = |items: &[&str]| items.iter().map(|&item| item.to_owned()).collect();
    let command = CustomCommand {
        makefile: interp.mf_index(),
        outputs: vec![output.to_owned()],
        depends: owned(depends),
        command_lines: vec![owned(line)],
        comment: comment.map(str::to_owned),
        backtrace: interp.backtrace.clone(),
        policies: interp.effective_policies(),
        ..Default::default()
    };
    if targets::validate_command_lines(interp, &command).is_err() {
        return;
    }
    mark_generated(interp, command.makefile, output);
    interp.ext.generator_actions.push(GeneratorAction::Output {
        command,
        replace: false,
    });
}

/// The full path of a wrapped file.
fn wrapped_path(interp: &Interp, name: &str, generated: bool) -> String {
    if is_full_path(name) {
        return name.to_owned();
    }
    let directory = if generated {
        interp.current_binary_dir()
    } else {
        interp.current_source_dir()
    };
    format!("{directory}/{name}")
}

fn append(list: &mut String, value: &str) {
    if !list.is_empty() {
        list.push(';');
    }
    list.push_str(value);
}

fn fltk_wrap_ui(interp: &mut Interp, args: &[String]) -> CmdResult {
    let [target, files @ ..] = args else {
        return err("called with incorrect number of arguments");
    };
    if files.is_empty() {
        return err("called with incorrect number of arguments");
    }
    let source_dir = interp.current_source_dir();
    let fluid = required_definition(interp, "FLTK_FLUID_EXECUTABLE");
    let binary_dir = interp.current_binary_dir();
    super::directory::add_include_directories(interp, &binary_dir, false);
    let mut sources = Vec::new();
    for file in files {
        if known_source(interp, file).is_some_and(|(excluded, ..)| excluded) {
            continue;
        }
        let name = filename_name(file);
        let name = name.split('.').next().unwrap_or_default();
        let header = format!("{binary_dir}/{name}.h");
        let source = format!("{binary_dir}/{name}.cxx");
        let origin = format!("{source_dir}/{file}");
        let line = [&*fluid, "-c", "-h", &header, "-o", &source, &origin];
        add_output(interp, &source, &[&origin, &fluid], &line, None);
        add_output(interp, &header, &[&origin, &fluid], &line, None);
        let dir = interp.mf_index();
        get_or_create_source(interp, dir, &source)
            .depends
            .extend([header, origin]);
        sources.push(collapse_full_path(&source, ""));
    }
    interp.set_def(format!("{target}_FLTK_UI_SRCS"), sources.join(";"));
    interp
        .ext
        .generator_actions
        .push(GeneratorAction::FltkTarget {
            makefile: interp.mf_index(),
            target: target.clone(),
            backtrace: interp.backtrace.clone(),
        });
    Ok(())
}

/// `FinalAction` of `cmFLTKWrapUICommand`.
pub(super) fn check_fltk_target(interp: &mut Interp, dir: usize, name: &str, backtrace: &[Frame]) {
    let local = interp.makefiles[dir]
        .targets
        .iter()
        .filter_map(|key| interp.ext.targets.get(key))
        .any(|target| target.name == name && !target.imported);
    if !local {
        let message = format!(
            "FLTK_WRAP_UI was called with a target that was never created: {name}.  The problem was found while processing the source directory: {}.  This FLTK_WRAP_UI call will be ignored.",
            interp.makefiles[dir].source_dir
        );
        interp.issue_at(MessageType::AuthorError, &message, backtrace);
    }
}

fn qt_wrap_cpp(interp: &mut Interp, args: &[String]) -> CmdResult {
    let [_library, list, files @ ..] = args else {
        return err("called with incorrect number of arguments");
    };
    if files.is_empty() {
        return err("called with incorrect number of arguments");
    }
    let moc = required_definition(interp, "QT_MOC_EXECUTABLE");
    let mut sources = interp.get_def(list).unwrap_or_default().to_owned();
    for file in files {
        let known = known_source(interp, file);
        if known.as_ref().is_some_and(|(excluded, ..)| *excluded) {
            continue;
        }
        let name = filename_without_last_extension(file);
        let output = format!("{}/moc_{name}.cxx", interp.current_binary_dir());
        let dir = interp.mf_index();
        mark_generated_ambiguous(interp, dir, &output);
        if let Some((_, Some(value), _)) = &known {
            get_or_create_source(interp, dir, &output)
                .props
                .set("ABSTRACT", value.clone());
        }
        let header = wrapped_path(interp, file, known.is_some_and(|(.., generated)| generated));
        append(&mut sources, &output);
        add_output(
            interp,
            &output,
            &[&moc, &header],
            &[&moc, "-o", &output, &header],
            Some("Qt Wrapped File"),
        );
    }
    interp.set_def(list.clone(), sources);
    Ok(())
}

fn qt_wrap_ui(interp: &mut Interp, args: &[String]) -> CmdResult {
    let [_library, header_list, source_list, files @ ..] = args else {
        return err("called with incorrect number of arguments");
    };
    if files.is_empty() {
        return err("called with incorrect number of arguments");
    }
    let uic = required_definition(interp, "QT_UIC_EXECUTABLE");
    let moc = required_definition(interp, "QT_MOC_EXECUTABLE");
    let mut headers = interp.get_def(header_list).unwrap_or_default().to_owned();
    let mut sources = interp.get_def(source_list).unwrap_or_default().to_owned();
    let binary_dir = interp.current_binary_dir();
    for file in files {
        let known = known_source(interp, file);
        if known.as_ref().is_some_and(|(excluded, ..)| *excluded) {
            continue;
        }
        let name = filename_without_last_extension(file);
        let header = format!("{binary_dir}/{name}.h");
        let source = format!("{binary_dir}/{name}.cxx");
        let moc_source = format!("{binary_dir}/moc_{name}.cxx");
        let ui = wrapped_path(interp, file, known.is_some_and(|(.., generated)| generated));
        append(&mut headers, &header);
        append(&mut sources, &source);
        append(&mut sources, &moc_source);
        add_output(interp, &header, &[&ui], &[&uic, "-o", &header, &ui], None);
        add_output(
            interp,
            &source,
            &[&ui, &header],
            &[&uic, "-impl", &header, "-o", &source, &ui],
            None,
        );
        add_output(
            interp,
            &moc_source,
            &[&header],
            &[&moc, "-o", &moc_source, &header],
            None,
        );
    }
    interp.set_def(source_list.clone(), sources);
    interp.set_def(header_list.clone(), headers);
    Ok(())
}

/// Has an effect on Windows hosts only.
fn include_external_msproject(_interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 2 {
        return err("INCLUDE_EXTERNAL_MSPROJECT called with incorrect number of arguments");
    }
    Ok(())
}
