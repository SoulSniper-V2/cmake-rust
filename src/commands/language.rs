//! Meta-operations which require control over the expansion of list-file arguments.

use std::collections::VecDeque;
use std::rc::Rc;

use crate::interp::{CmdErr, CmdResult, Ctl, Deferred, Interp, MacroSubst, Mode, ParsedFile, err};
use crate::lexer::{Arg, Command, Delim};
use crate::messages::MessageType;
use crate::properties::find_directory;
use crate::util::{collapse_full_path, strtol};

pub fn register(interp: &mut Interp) {
    interp.register_raw("cmake_language", language);
}

struct Arguments<'a> {
    file: &'a str,
    raw: &'a [Arg],
    expanded: VecDeque<String>,
}

impl Arguments<'_> {
    fn next(&mut self, interp: &mut Interp) -> Option<String> {
        while self.expanded.is_empty() {
            let (arg, remaining) = self.raw.split_first()?;
            self.raw = remaining;
            self.expanded.extend(
                interp
                    .expand_arguments(self.file, std::slice::from_ref(arg), None)?
                    .into_iter()
                    .map(|(value, _)| value),
            );
        }
        self.expanded.pop_front()
    }

    fn finish(&mut self, interp: &mut Interp) -> Vec<String> {
        let mut args: Vec<_> = self.expanded.drain(..).collect();
        if let Some(rest) = interp.expand_arguments(self.file, self.raw, None) {
            args.extend(rest.into_iter().map(|(value, _)| value));
        }
        self.raw = &[];
        args
    }
}

fn language(
    interp: &mut Interp,
    file: &Rc<ParsedFile>,
    command: &Command,
    subst: Option<&MacroSubst>,
) -> (bool, Ctl) {
    let mut raw = command.args.clone();
    if let Some(subst) = subst {
        for arg in &mut raw {
            if arg.delim != Delim::Bracket {
                arg.value = subst.apply(&arg.value);
            }
        }
    }
    let mut args = Arguments {
        file: &file.path,
        raw: &raw,
        expanded: VecDeque::new(),
    };
    match dispatch(interp, file, command, &mut args) {
        Ok(ctl) => (true, ctl),
        Err(error) => {
            if let CmdErr::Error(error) = error {
                if !interp.fatal {
                    interp.issue(
                        MessageType::FatalError,
                        &format!("{} {error}", command.name),
                    );
                }
                interp.set_fatal();
            }
            (false, Ctl::Normal)
        }
    }
}

fn dispatch(
    interp: &mut Interp,
    file: &Rc<ParsedFile>,
    command: &Command,
    args: &mut Arguments<'_>,
) -> Result<Ctl, CmdErr> {
    let operation = args
        .next(interp)
        .ok_or("called with incorrect number of arguments")?;
    match operation.as_str() {
        "CALL" => call(interp, file, command, args, None),
        "DEFER" => defer(interp, file, command, args),
        "EVAL" => {
            let rest = args.finish(interp);
            match rest.first().map(String::as_str) {
                None => return err("called with incorrect number of arguments"),
                Some("CODE") => {}
                _ if rest.iter().any(|arg| arg == "CODE") => {
                    return err(
                        "called with unsupported arguments between EVAL and CODE arguments",
                    );
                }
                _ => return err("called without CODE argument"),
            }
            let path = format!("{}:{}:EVAL", file.path, command.line);
            if !interp.run_string(&rest[1..].join(" "), &path) {
                interp.issue(MessageType::FatalError, "cmake_language unknown error.");
                interp.set_fatal_if_script();
                return Err(CmdErr::Reported);
            }
            Ok(Ctl::Normal)
        }
        "EXIT" => {
            let value = args.next(interp).ok_or("EXIT requires one argument")?;
            if interp.mode != Mode::Script {
                return err("EXIT can be used only in SCRIPT mode");
            }
            let (code, valid) = strtol(&value, 10);
            if !valid
                || value
                    .trim_start_matches(|ch: char| ch.is_ascii_whitespace())
                    .parse::<i64>()
                    .is_err()
            {
                return err(format!(
                    "EXIT requires one integral argument, got \"{value}\""
                ));
            }
            interp.exit_code = Some(code as i32);
            Ok(Ctl::Normal)
        }
        "SET_DEPENDENCY_PROVIDER" => {
            let rest = args.finish(interp);
            set_dependency_provider(interp, &rest)?;
            Ok(Ctl::Normal)
        }
        "GET_MESSAGE_LOG_LEVEL" => {
            let rest = args.finish(interp);
            let [variable] = rest.as_slice() else {
                return err("sub-command GET_MESSAGE_LOG_LEVEL expects exactly one argument");
            };
            interp.set_def(variable, super::message::current_log_level(interp).as_str());
            Ok(Ctl::Normal)
        }
        "GET_EXPERIMENTAL_FEATURE_ENABLED" => {
            let rest = args.finish(interp);
            let [feature, variable] = rest.as_slice() else {
                return err(
                    "sub-command GET_EXPERIMENTAL_FEATURE_ENABLED expects exactly two arguments",
                );
            };
            let Some((_, uuid, option, description)) = EXPERIMENTAL_FEATURES
                .iter()
                .find(|(name, ..)| *name == feature)
            else {
                return err(format!(
                    "Experimental feature name \"{feature}\" does not exist."
                ));
            };
            let enabled = interp.read_definition(option).is_some_and(|value| {
                let enabled = value == *uuid;
                if interp.ext.experimental_warnings.insert((feature.clone(), value.clone())) {
                    let warning = if enabled {
                        (*description).to_owned()
                    } else {
                        format!("{option} is set to incorrect value\n  {value}\nSee 'Help/dev/experimental.rst' in the source tree of this version of CMake for documentation of the experimental feature and the corresponding activation value.  This project's code may require changes to work with this CMake's version of the feature.")
                    };
                    interp.issue(MessageType::AuthorWarning, &warning);
                }
                enabled
            });
            interp.set_def(variable, if enabled { "TRUE" } else { "FALSE" });
            Ok(Ctl::Normal)
        }
        _ => err("called with unknown meta-operation"),
    }
}

const EXPERIMENTAL_FEATURES: &[(&str, &str, &str, &str)] = &[
    (
        "ExportPackageDependencies",
        "1942b4fa-b2c5-4546-9385-83f254070067",
        "CMAKE_EXPERIMENTAL_EXPORT_PACKAGE_DEPENDENCIES",
        "CMake's EXPORT_PACKAGE_DEPENDENCIES support is experimental. It is meant only for experimentation and feedback to CMake developers.",
    ),
    (
        "CxxImportStd",
        "d0edc3af-4c50-42ea-a356-e2862fe7a444",
        "CMAKE_EXPERIMENTAL_CXX_IMPORT_STD",
        "CMake's support for `import std;` in C++23 and newer is experimental. It is meant only for experimentation and feedback to CMake developers.",
    ),
    (
        "ImportPackageInfo",
        "e82e467b-f997-4464-8ace-b00808fff261",
        "CMAKE_EXPERIMENTAL_FIND_CPS_PACKAGES",
        "CMake's support for importing package information in the Common Package Specification format (via find_package) is experimental. It is meant only for experimentation and feedback to CMake developers.",
    ),
    (
        "ExportPackageInfo",
        "b80be207-778e-46ba-8080-b23bba22639e",
        "CMAKE_EXPERIMENTAL_EXPORT_PACKAGE_INFO",
        "CMake's support for exporting package information in the Common Package Specification format is experimental. It is meant only for experimentation and feedback to CMake developers.",
    ),
    (
        "ExportBuildDatabase",
        "73194a1d-c0b5-41b9-9190-a4512925e192",
        "CMAKE_EXPERIMENTAL_EXPORT_BUILD_DATABASE",
        "CMake's support for exporting build databases is experimental. It is meant only for experimentation and feedback to CMake developers.",
    ),
    (
        "Instrumentation",
        "a37d1069-1972-4901-b9c9-f194aaf2b6e0",
        "CMAKE_EXPERIMENTAL_INSTRUMENTATION",
        "CMake's support for collecting instrumentation data is experimental. It is meant only for experimentation and feedback to CMake developers.",
    ),
];

#[derive(Default)]
struct Defer {
    directory: Option<usize>,
    id: Option<String>,
    id_var: Option<String>,
}

fn active(interp: &Interp, directory: usize) -> bool {
    interp.mode == Mode::Project && interp.mf_stack.contains(&directory)
}

fn upper(value: &str) -> bool {
    value.as_bytes().first().is_some_and(u8::is_ascii_uppercase)
}

fn defer(
    interp: &mut Interp,
    file: &Rc<ParsedFile>,
    command: &Command,
    args: &mut Arguments<'_>,
) -> Result<Ctl, CmdErr> {
    let mut options = Defer::default();
    let mut arg = args
        .next(interp)
        .ok_or("DEFER requires at least one argument")?;
    loop {
        match arg.as_str() {
            "CALL" => return call(interp, file, command, args, Some(options)),
            "CANCEL_CALL" | "GET_CALL_IDS" | "GET_CALL" => {
                if options.id.is_some() || options.id_var.is_some() {
                    return err(format!("DEFER {arg} does not accept ID or ID_VAR."));
                }
                let directory = options.directory.unwrap_or_else(|| interp.mf_index());
                let tail = args.finish(interp);
                defer_query(interp, directory, &arg, &tail)?;
                return Ok(Ctl::Normal);
            }
            "DIRECTORY" => {
                if options.directory.is_some() {
                    return err("DEFER given multiple DIRECTORY arguments");
                }
                let dir = args.next(interp).ok_or("DEFER DIRECTORY missing value")?;
                if dir.is_empty() {
                    return err("DEFER DIRECTORY may not be empty");
                }
                options.directory = Some(find_directory(interp, &dir).ok_or_else(|| {
                    let full = collapse_full_path(&dir, &interp.current_source_dir());
                    format!("DEFER DIRECTORY:\n  {full}\nis not known.  It may not have been processed yet.")
                })?);
            }
            "ID" => {
                if options.id.is_some() {
                    return err("DEFER given multiple ID arguments");
                }
                let id = args.next(interp).ok_or("DEFER ID missing value")?;
                if id.is_empty() {
                    return err("DEFER ID may not be empty");
                }
                if upper(&id) {
                    return err("DEFER ID may not start in A-Z.");
                }
                options.id = Some(id);
            }
            "ID_VAR" => {
                if options.id_var.is_some() {
                    return err("DEFER given multiple ID_VAR arguments");
                }
                let id_var = args
                    .next(interp)
                    .ok_or("DEFER ID_VAR missing variable name")?;
                if id_var.is_empty() {
                    return err("DEFER ID_VAR may not be empty");
                }
                options.id_var = Some(id_var);
            }
            _ => return err(format!("DEFER unknown option:\n  {arg}")),
        }
        arg = args
            .next(interp)
            .ok_or("DEFER must be followed by a CALL argument")?;
    }
}

fn defer_query(
    interp: &mut Interp,
    directory: usize,
    operation: &str,
    args: &[String],
) -> CmdResult {
    let dir = &interp.makefiles[directory].binary_dir;
    let accessible = active(interp, directory);
    match operation {
        "GET_CALL_IDS" => {
            let var = args
                .first()
                .ok_or("DEFER GET_CALL_IDS missing output variable")?;
            if args.len() != 1 {
                return err("DEFER GET_CALL_IDS given too many arguments");
            }
            if !accessible {
                return err(format!(
                    "DEFER GET_CALL_IDS may not access directory:\n  {dir}\nat this time."
                ));
            }
            let ids = interp.makefiles[directory]
                .deferred
                .iter()
                .map(|call| call.id.as_str())
                .collect::<Vec<_>>()
                .join(";");
            interp.set_def(var, ids);
        }
        "GET_CALL" => {
            let id = args.first().ok_or("DEFER GET_CALL missing id")?;
            let var = args
                .get(1)
                .ok_or("DEFER GET_CALL missing output variable")?;
            if args.len() != 2 {
                return err("DEFER GET_CALL given too many arguments");
            }
            if id.is_empty() {
                return err("DEFER GET_CALL id may not be empty");
            }
            if upper(id) {
                return err(format!("DEFER GET_CALL unknown argument:\n {id}"));
            }
            if !accessible {
                return err(format!(
                    "DEFER GET_CALL may not access directory:\n  {dir}\nat this time."
                ));
            }
            let call = interp.makefiles[directory]
                .deferred
                .iter()
                .find(|call| &call.id == id)
                .map(|call| {
                    std::iter::once(call.command.name.as_str())
                        .chain(call.command.args.iter().map(|arg| arg.value.as_str()))
                        .collect::<Vec<_>>()
                        .join(";")
                })
                .unwrap_or_default();
            interp.set_def(var, call);
        }
        _ => {
            for id in args {
                if upper(id) {
                    return err(format!("DEFER CANCEL_CALL unknown argument:\n  {id}"));
                }
                if !accessible {
                    return err(format!(
                        "DEFER CANCEL_CALL may not update directory:\n  {dir}\nat this time."
                    ));
                }
            }
            interp.makefiles[directory]
                .deferred
                .retain(|call| !args.contains(&call.id));
        }
    }
    Ok(())
}

fn call(
    interp: &mut Interp,
    file: &Rc<ParsedFile>,
    caller: &Command,
    args: &mut Arguments<'_>,
    defer: Option<Defer>,
) -> Result<Ctl, CmdErr> {
    let name = args.next(interp).ok_or("CALL missing command name")?;
    if !args.expanded.is_empty() {
        return err("CALL command's arguments must be literal");
    }
    let lname = name.to_ascii_lowercase();
    if matches!(
        lname.as_str(),
        "if" | "elseif"
            | "else"
            | "endif"
            | "while"
            | "endwhile"
            | "foreach"
            | "endforeach"
            | "function"
            | "endfunction"
            | "macro"
            | "endmacro"
            | "block"
            | "endblock"
    ) || (defer.is_some() && lname == "return")
    {
        return err(format!("invalid command specified: {name}"));
    }
    let command = Command {
        name,
        lname,
        line: caller.line,
        line_end: caller.line,
        args: args
            .raw
            .iter()
            .map(|arg| Arg {
                line: caller.line,
                ..arg.clone()
            })
            .collect(),
    };
    if let Some(options) = defer {
        let id = options.id.unwrap_or_else(|| {
            let id = format!("__{}", interp.ext.next_defer_id);
            interp.ext.next_defer_id += 1;
            id
        });
        if let Some(variable) = options.id_var {
            interp.set_def(variable, &id);
        }
        let directory = options.directory.unwrap_or_else(|| interp.mf_index());
        if !active(interp, directory) {
            let dir = &interp.makefiles[directory].binary_dir;
            return err(format!(
                "DEFER CALL may not be scheduled in directory:\n  {dir}\nat this time."
            ));
        }
        interp.makefiles[directory].deferred.push(Deferred {
            id,
            file: file.path.clone(),
            command,
        });
        return Ok(Ctl::Normal);
    }
    let (ok, ctl) = interp.execute(file, &command, None);
    if ok { Ok(ctl) } else { Err(CmdErr::Reported) }
}

fn set_dependency_provider(interp: &mut Interp, args: &[String]) -> CmdResult {
    if !interp.ext.in_top_level_includes {
        return err(
            "Dependency providers can only be set as part of the first call to project(). More specifically, cmake_language(SET_DEPENDENCY_PROVIDER) can only be called while the first project() command processes files listed in CMAKE_PROJECT_TOP_LEVEL_INCLUDES.",
        );
    }
    let mut parts = args.splitn(2, |arg| arg == "SUPPORTED_METHODS");
    let command = parts.next().unwrap_or_default();
    let methods = parts.next().unwrap_or_default();
    if let Some(extra) = command.get(1) {
        return err(format!("Unrecognized keyword: \"{extra}\""));
    }
    let name = command.first().map_or("", String::as_str);
    let property = "__FETCHCONTENT_MAKEAVAILABLE_SERIAL_PROVIDER";
    if name.is_empty() {
        if !methods.is_empty() {
            return err("Must specify a non-empty command name when provider methods are given");
        }
        interp.ext.dependency_provider = None;
        interp.global_props.set(property, String::new());
        return Ok(());
    }
    if !interp.has_command(name) {
        return err(format!("Command \"{name}\" is not a defined command"));
    }
    if methods.is_empty() {
        return err("Must specify at least one provider method");
    }
    for method in methods {
        if !matches!(
            method.as_str(),
            "FIND_PACKAGE" | "FETCHCONTENT_MAKEAVAILABLE_SERIAL"
        ) {
            return err(format!("Unknown dependency provider method \"{method}\""));
        }
    }
    let fetch = methods
        .iter()
        .any(|method| method == "FETCHCONTENT_MAKEAVAILABLE_SERIAL");
    interp.global_props.set(
        property,
        if fetch {
            name.to_owned()
        } else {
            String::new()
        },
    );
    interp.ext.dependency_provider = Some((name.to_owned(), methods.to_vec()));
    Ok(())
}
