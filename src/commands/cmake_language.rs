//! `cmake_language` (cmCMakeLanguageCommand.cxx).
//!
//! Unlike ordinary builtins this command expands its arguments lazily (one
//! raw argument at a time) because `CALL` forwards the remaining arguments
//! *unexpanded* to the invoked command. It is therefore registered with
//! `register_raw` and receives the `ParsedFile`/`Command` it was called from.

use std::rc::Rc;

use crate::interp::{CmdErr, Ctl, Interp, MacroSubst, Mode, ParsedFile, err};
use crate::lexer::{Arg, Command};
use crate::messages::MessageType;
use crate::util;

pub fn register(interp: &mut Interp) {
    interp.register_raw("cmake_language", cmake_language);
}

/// `status.SetError(...)` plus `cmSystemTools::SetFatalErrorOccurred()`.
fn fatal<T>(interp: &mut Interp, message: impl Into<String>) -> Result<T, CmdErr> {
    interp.set_fatal();
    err(message)
}

const INVALID_COMMANDS: [&str; 14] = [
    "function",
    "endfunction",
    "macro",
    "endmacro",
    "if",
    "elseif",
    "else",
    "endif",
    "while",
    "endwhile",
    "foreach",
    "endforeach",
    "block",
    "endblock",
];

const INVALID_DEFER_COMMANDS: [&str; 1] = ["return"];

#[derive(Default)]
struct Defer {
    id: String,
    id_var: String,
    directory: Option<usize>,
}

/// Expand one more raw argument into `exp` (an unquoted argument may expand
/// to zero or several values). Returns false when no raw arguments remain.
fn more_args(
    interp: &mut Interp,
    file: &Rc<ParsedFile>,
    args: &[Arg],
    subst: Option<&MacroSubst>,
    exp: &mut Vec<String>,
    raw: &mut usize,
    exp_arg: usize,
) -> bool {
    while exp_arg >= exp.len() {
        if *raw >= args.len() {
            return false;
        }
        if let Some(values) = interp.expand_arguments(&file.path, &args[*raw..*raw + 1], subst) {
            exp.extend(values.into_iter().map(|(value, _)| value));
        }
        *raw += 1;
    }
    true
}

/// `finishArgs`: expand all remaining raw arguments into `exp`.
fn finish_args(
    interp: &mut Interp,
    file: &Rc<ParsedFile>,
    args: &[Arg],
    subst: Option<&MacroSubst>,
    exp: &mut Vec<String>,
    raw: &mut usize,
) {
    if let Some(values) = interp.expand_arguments(&file.path, &args[*raw..], subst) {
        exp.extend(values.into_iter().map(|(value, _)| value));
    }
    *raw = args.len();
}

/// Expand every raw argument (EVAL, GET_MESSAGE_LOG_LEVEL, ...).
fn expand_all(
    interp: &mut Interp,
    file: &Rc<ParsedFile>,
    args: &[Arg],
    subst: Option<&MacroSubst>,
) -> Vec<String> {
    interp
        .expand_arguments(&file.path, args, subst)
        .unwrap_or_default()
        .into_iter()
        .map(|(value, _)| value)
        .collect()
}

/// The `cmListFileContext` of the `cmake_language` call (top backtrace frame).
fn context(interp: &Interp, file: &Rc<ParsedFile>) -> (Rc<str>, usize) {
    match interp.backtrace.last() {
        Some(frame) => (frame.file.clone(), frame.line),
        None => (file.path.clone(), 0),
    }
}

fn cmake_language(
    interp: &mut Interp,
    file: &Rc<ParsedFile>,
    command: &Command,
    subst: Option<&MacroSubst>,
) -> Result<Ctl, CmdErr> {
    let args = &command.args;
    let mut exp: Vec<String> = Vec::new();
    let mut raw = 0usize;
    let mut ei = 0usize;

    if !more_args(interp, file, args, subst, &mut exp, &mut raw, ei) {
        return fatal(interp, "called with incorrect number of arguments");
    }

    if exp[ei] == "EXIT" {
        ei += 1; // consume "EXIT".
        if !more_args(interp, file, args, subst, &mut exp, &mut raw, ei) {
            return fatal(interp, "EXIT requires one argument");
        }
        if interp.mode != Mode::Script {
            return fatal(interp, "EXIT can be used only in SCRIPT mode");
        }
        let (code, ok) = util::strtol(&exp[ei], 10);
        if !ok {
            return fatal(
                interp,
                format!("EXIT requires one integral argument, got \"{}\"", exp[ei]),
            );
        }
        interp.exit_code = Some(code as i32);
        return Ok(Ctl::Normal);
    }

    if exp[ei] == "SET_DEPENDENCY_PROVIDER" {
        finish_args(interp, file, args, subst, &mut exp, &mut raw);
        return set_dependency_provider(interp, &exp).map(|()| Ctl::Normal);
    }

    let mut maybe_defer: Option<Defer> = None;
    if exp[ei] == "DEFER" {
        ei += 1; // Consume "DEFER".
        if !more_args(interp, file, args, subst, &mut exp, &mut raw, ei) {
            return fatal(interp, "DEFER requires at least one argument");
        }
        let mut defer = Defer::default();

        // Process optional arguments.
        while more_args(interp, file, args, subst, &mut exp, &mut raw, ei) {
            if exp[ei] == "CALL" {
                break;
            }
            if exp[ei] == "CANCEL_CALL" || exp[ei] == "GET_CALL_IDS" || exp[ei] == "GET_CALL" {
                if !defer.id.is_empty() || !defer.id_var.is_empty() {
                    return fatal(
                        interp,
                        format!("DEFER {} does not accept ID or ID_VAR.", exp[ei]),
                    );
                }
                finish_args(interp, file, args, subst, &mut exp, &mut raw);
                return defer_operation(interp, &defer, &exp, ei).map(|()| Ctl::Normal);
            }
            if exp[ei] == "DIRECTORY" {
                ei += 1; // Consume "DIRECTORY".
                if defer.directory.is_some() {
                    return fatal(interp, "DEFER given multiple DIRECTORY arguments");
                }
                if !more_args(interp, file, args, subst, &mut exp, &mut raw, ei) {
                    return fatal(interp, "DEFER DIRECTORY missing value");
                }
                let dir = exp[ei].clone();
                ei += 1;
                if dir.is_empty() {
                    return fatal(interp, "DEFER DIRECTORY may not be empty");
                }
                let dir = util::collapse_full_path(&dir, &interp.current_source_dir());
                defer.directory = interp.find_makefile(&dir);
                if defer.directory.is_none() {
                    return fatal(
                        interp,
                        format!(
                            "DEFER DIRECTORY:\n  {dir}\nis not known.  \
                             It may not have been processed yet."
                        ),
                    );
                }
            } else if exp[ei] == "ID" {
                ei += 1; // Consume "ID".
                if !defer.id.is_empty() {
                    return fatal(interp, "DEFER given multiple ID arguments");
                }
                if !more_args(interp, file, args, subst, &mut exp, &mut raw, ei) {
                    return fatal(interp, "DEFER ID missing value");
                }
                defer.id = exp[ei].clone();
                ei += 1;
                if defer.id.is_empty() {
                    return fatal(interp, "DEFER ID may not be empty");
                }
                if defer.id.starts_with(|c: char| c.is_ascii_uppercase()) {
                    return fatal(interp, "DEFER ID may not start in A-Z.");
                }
            } else if exp[ei] == "ID_VAR" {
                ei += 1; // Consume "ID_VAR".
                if !defer.id_var.is_empty() {
                    return fatal(interp, "DEFER given multiple ID_VAR arguments");
                }
                if !more_args(interp, file, args, subst, &mut exp, &mut raw, ei) {
                    return fatal(interp, "DEFER ID_VAR missing variable name");
                }
                defer.id_var = exp[ei].clone();
                ei += 1;
                if defer.id_var.is_empty() {
                    return fatal(interp, "DEFER ID_VAR may not be empty");
                }
            } else {
                return fatal(interp, format!("DEFER unknown option:\n  {}", exp[ei]));
            }
        }

        if !(more_args(interp, file, args, subst, &mut exp, &mut raw, ei) && exp[ei] == "CALL") {
            return fatal(interp, "DEFER must be followed by a CALL argument");
        }

        maybe_defer = Some(defer);
    }

    if exp[ei] == "CALL" {
        ei += 1; // Consume "CALL".

        // CALL requires a command name.
        if !more_args(interp, file, args, subst, &mut exp, &mut raw, ei) {
            return fatal(interp, "CALL missing command name");
        }
        let call_command = exp[ei].clone();
        ei += 1;

        // CALL accepts no further expanded arguments.
        if ei != exp.len() {
            return fatal(interp, "CALL command's arguments must be literal");
        }

        // Run the CALL.
        return call(interp, file, args, &call_command, raw, maybe_defer, subst);
    }

    if exp[ei] == "EVAL" {
        return eval_code(interp, file, args, subst).map(|()| Ctl::Normal);
    }

    if exp[ei] == "GET_MESSAGE_LOG_LEVEL" {
        return get_message_log_level(interp, file, args, subst).map(|()| Ctl::Normal);
    }

    if exp[ei] == "GET_EXPERIMENTAL_FEATURE_ENABLED" {
        return get_experimental_feature(interp, file, args, subst).map(|()| Ctl::Normal);
    }

    fatal(interp, "called with unknown meta-operation")
}

/// `cmCMakeLanguageCommandCALL`: invoke `call_command` with the remaining
/// *raw* arguments, now or deferred to the end of the directory.
fn call(
    interp: &mut Interp,
    file: &Rc<ParsedFile>,
    args: &[Arg],
    call_command: &str,
    start_arg: usize,
    defer: Option<Defer>,
    subst: Option<&MacroSubst>,
) -> Result<Ctl, CmdErr> {
    // Ensure the specified command is valid; start/end flow control commands
    // are not allowed.
    let lname = call_command.to_lowercase();
    if INVALID_COMMANDS.contains(&lname.as_str())
        || (defer.is_some() && INVALID_DEFER_COMMANDS.contains(&lname.as_str()))
    {
        return fatal(interp, format!("invalid command specified: {call_command}"));
    }

    let (ctx_file, ctx_line) = context(interp, file);

    // The rest of the arguments are passed to the function call.
    let func_args: Vec<Arg> = args[start_arg..]
        .iter()
        .map(|arg| Arg {
            value: arg.value.clone(),
            delim: arg.delim,
            line: ctx_line,
        })
        .collect();
    let func = Command {
        name: call_command.to_owned(),
        lname,
        line: ctx_line,
        line_end: ctx_line,
        args: func_args,
    };

    if let Some(mut defer) = defer {
        if defer.id.is_empty() {
            defer.id = interp.new_defer_id();
        }
        if !defer.id_var.is_empty() {
            interp.set_def(defer.id_var.clone(), defer.id.clone());
        }
        let index = defer.directory.unwrap_or_else(|| interp.mf_index());
        if !interp.defer_call(index, defer.id, ctx_file.to_string(), func) {
            return fatal(
                interp,
                format!(
                    "DEFER CALL may not be scheduled in directory:\n  {}\nat this time.",
                    interp.makefiles[index].binary_dir
                ),
            );
        }
        return Ok(Ctl::Normal);
    }
    // `cmMakefile::ExecuteCommand`.
    let (ok, ctl) = interp.execute(file, &func, subst);
    if !ok {
        return Err(CmdErr::Reported);
    }
    Ok(ctl)
}

/// `cmCMakeLanguageCommandDEFER` for CANCEL_CALL/GET_CALL_IDS/GET_CALL.
fn defer_operation(
    interp: &mut Interp,
    defer: &Defer,
    args: &[String],
    mut arg: usize,
) -> Result<(), CmdErr> {
    let index = defer.directory.unwrap_or_else(|| interp.mf_index());
    let binary_dir = |interp: &Interp| interp.makefiles[index].binary_dir.clone();
    match args[arg].as_str() {
        "CANCEL_CALL" => {
            arg += 1; // Consume CANCEL_CALL.
            for id in &args[arg..] {
                if id.starts_with(|c: char| c.is_ascii_uppercase()) {
                    return fatal(
                        interp,
                        format!("DEFER CANCEL_CALL unknown argument:\n  {id}"),
                    );
                }
                if !interp.defer_cancel_call(index, id) {
                    return fatal(
                        interp,
                        format!(
                            "DEFER CANCEL_CALL may not update directory:\n  {}\nat this time.",
                            binary_dir(interp)
                        ),
                    );
                }
            }
            Ok(())
        }
        "GET_CALL_IDS" => {
            arg += 1; // Consume GET_CALL_IDS.
            if arg == args.len() {
                return fatal(interp, "DEFER GET_CALL_IDS missing output variable");
            }
            let var = args[arg].clone();
            arg += 1;
            if arg != args.len() {
                return fatal(interp, "DEFER GET_CALL_IDS given too many arguments");
            }
            let Some(ids) = interp.defer_get_call_ids(index) else {
                return fatal(
                    interp,
                    format!(
                        "DEFER GET_CALL_IDS may not access directory:\n  {}\nat this time.",
                        binary_dir(interp)
                    ),
                );
            };
            interp.set_def(var, ids);
            Ok(())
        }
        "GET_CALL" => {
            arg += 1; // Consume GET_CALL.
            if arg == args.len() {
                return fatal(interp, "DEFER GET_CALL missing id");
            }
            let id = args[arg].clone();
            arg += 1;
            if arg == args.len() {
                return fatal(interp, "DEFER GET_CALL missing output variable");
            }
            let var = args[arg].clone();
            arg += 1;
            if arg != args.len() {
                return fatal(interp, "DEFER GET_CALL given too many arguments");
            }
            if id.is_empty() {
                return fatal(interp, "DEFER GET_CALL id may not be empty");
            }
            if id.starts_with(|c: char| c.is_ascii_uppercase()) {
                return fatal(interp, format!("DEFER GET_CALL unknown argument:\n {id}"));
            }
            let Some(call) = interp.defer_get_call(index, &id) else {
                return fatal(
                    interp,
                    format!(
                        "DEFER GET_CALL may not access directory:\n  {}\nat this time.",
                        binary_dir(interp)
                    ),
                );
            };
            interp.set_def(var, call);
            Ok(())
        }
        other => fatal(interp, format!("DEFER operation unknown: {other}")),
    }
}

/// `cmCMakeLanguageCommandEVAL`: run `EVAL CODE <code>`.
fn eval_code(
    interp: &mut Interp,
    file: &Rc<ParsedFile>,
    args: &[Arg],
    subst: Option<&MacroSubst>,
) -> Result<(), CmdErr> {
    let expanded = expand_all(interp, file, args, subst);
    if expanded.len() < 2 {
        return fatal(interp, "called with incorrect number of arguments");
    }
    if expanded[1] != "CODE" {
        if !expanded[2..].iter().any(|arg| arg == "CODE") {
            return fatal(interp, "called without CODE argument");
        }
        return fatal(
            interp,
            "called with unsupported arguments between EVAL and CODE arguments",
        );
    }
    let code = expanded[2..].join(" ");
    let (ctx_file, ctx_line) = context(interp, file);
    let name = format!("{ctx_file}:{ctx_line}:EVAL");
    if !interp.run_string(&code, &name) {
        // Parse failure: diagnostics were issued below the status stack, so
        // `status.GetError()` is still the default "unknown error.".
        return err("unknown error.");
    }
    Ok(())
}

/// `cmCMakeLanguageCommandSET_DEPENDENCY_PROVIDER`.
fn set_dependency_provider(interp: &mut Interp, args: &[String]) -> Result<(), CmdErr> {
    if !interp.ext.in_top_level_includes {
        return fatal(
            interp,
            "Dependency providers can only be set as part of the first call to \
             project(). More specifically, cmake_language(SET_DEPENDENCY_PROVIDER) \
             can only be called while the first project() command processes files \
             listed in CMAKE_PROJECT_TOP_LEVEL_INCLUDES.",
        );
    }

    // `cmArgumentParser`: "SET_DEPENDENCY_PROVIDER" binds the following value
    // as the provider command; "SUPPORTED_METHODS" binds a non-empty list.
    let mut command = String::new();
    let mut methods: &[String] = &[];
    let mut unparsed: Vec<&String> = Vec::new();
    let mut i = 1;
    if i < args.len() {
        command = args[i].clone();
        i += 1;
    }
    while i < args.len() {
        if args[i] == "SUPPORTED_METHODS" {
            methods = &args[i + 1..];
            break;
        }
        unparsed.push(&args[i]);
        i += 1;
    }
    if !unparsed.is_empty() {
        return fatal(interp, format!("Unrecognized keyword: \"{}\"", unparsed[0]));
    }

    const FCMAS_PROP: &str = "__FETCHCONTENT_MAKEAVAILABLE_SERIAL_PROVIDER";

    if command.is_empty() {
        if !methods.is_empty() {
            return fatal(
                interp,
                "Must specify a non-empty command name when provider methods are given",
            );
        }
        interp.ext.dependency_provider = None;
        interp.global_props.set(FCMAS_PROP, String::new());
        return Ok(());
    }

    if !interp.commands.contains_key(&command.to_lowercase()) {
        return fatal(
            interp,
            format!("Command \"{command}\" is not a defined command"),
        );
    }

    if methods.is_empty() {
        return fatal(interp, "Must specify at least one provider method");
    }

    let mut find_package = false;
    let mut serial = false;
    for method in methods {
        match method.as_str() {
            "FIND_PACKAGE" => find_package = true,
            "FETCHCONTENT_MAKEAVAILABLE_SERIAL" => serial = true,
            _ => {
                return fatal(
                    interp,
                    format!("Unknown dependency provider method \"{method}\""),
                );
            }
        }
    }

    interp.ext.dependency_provider = Some(crate::state::DependencyProvider {
        command: command.clone(),
        find_package,
        fetchcontent_makeavailable_serial: serial,
    });
    interp
        .global_props
        .set(FCMAS_PROP, if serial { command } else { String::new() });
    Ok(())
}

/// `cmCMakeLanguageCommandGET_MESSAGE_LOG_LEVEL`.
fn get_message_log_level(
    interp: &mut Interp,
    file: &Rc<ParsedFile>,
    args: &[Arg],
    subst: Option<&MacroSubst>,
) -> Result<(), CmdErr> {
    let expanded = expand_all(interp, file, args, subst);
    if args.len() < 2 || expanded.len() > 2 {
        return fatal(
            interp,
            "sub-command GET_MESSAGE_LOG_LEVEL expects exactly one argument",
        );
    }
    use crate::commands::core::LogLevel;
    let level = match crate::commands::core::current_log_level(interp) {
        LogLevel::Error => "ERROR",
        LogLevel::Warning => "WARNING",
        LogLevel::Notice => "NOTICE",
        LogLevel::Status => "STATUS",
        LogLevel::Verbose => "VERBOSE",
        LogLevel::Debug => "DEBUG",
        LogLevel::Trace => "TRACE",
    };
    interp.set_def(expanded[1].clone(), level);
    Ok(())
}

/// `cmExperimental::Feature` lookup table (name, uuid, variable,
/// AUTHOR_WARNING description).
const EXPERIMENTAL_FEATURES: [(&str, &str, &str, &str); 6] = [
    (
        "ExportPackageDependencies",
        "1942b4fa-b2c5-4546-9385-83f254070067",
        "CMAKE_EXPERIMENTAL_EXPORT_PACKAGE_DEPENDENCIES",
        "CMake's EXPORT_PACKAGE_DEPENDENCIES support is experimental. It is \
         meant only for experimentation and feedback to CMake developers.",
    ),
    (
        "CxxImportStd",
        "d0edc3af-4c50-42ea-a356-e2862fe7a444",
        "CMAKE_EXPERIMENTAL_CXX_IMPORT_STD",
        "CMake's support for `import std;` in C++23 and newer is experimental. \
         It is meant only for experimentation and feedback to CMake developers.",
    ),
    (
        "ImportPackageInfo",
        "e82e467b-f997-4464-8ace-b00808fff261",
        "CMAKE_EXPERIMENTAL_FIND_CPS_PACKAGES",
        "CMake's support for importing package information in the Common \
         Package Specification format (via find_package) is experimental. It \
         is meant only for experimentation and feedback to CMake developers.",
    ),
    (
        "ExportPackageInfo",
        "b80be207-778e-46ba-8080-b23bba22639e",
        "CMAKE_EXPERIMENTAL_EXPORT_PACKAGE_INFO",
        "CMake's support for exporting package information in the Common \
         Package Specification format is experimental. It is meant only for \
         experimentation and feedback to CMake developers.",
    ),
    (
        "ExportBuildDatabase",
        "73194a1d-c0b5-41b9-9190-a4512925e192",
        "CMAKE_EXPERIMENTAL_EXPORT_BUILD_DATABASE",
        "CMake's support for exporting build databases is experimental. It is \
         meant only for experimentation and feedback to CMake developers.",
    ),
    (
        "Instrumentation",
        "a37d1069-1972-4901-b9c9-f194aaf2b6e0",
        "CMAKE_EXPERIMENTAL_INSTRUMENTATION",
        "CMake's support for collecting instrumentation data is experimental. \
         It is meant only for experimentation and feedback to CMake developers.",
    ),
];

/// `cmCMakeLanguageCommandGET_EXPERIMENTAL_FEATURE_ENABLED` and
/// `cmExperimental::HasSupportEnabled`.
fn get_experimental_feature(
    interp: &mut Interp,
    file: &Rc<ParsedFile>,
    args: &[Arg],
    subst: Option<&MacroSubst>,
) -> Result<(), CmdErr> {
    let expanded = expand_all(interp, file, args, subst);
    if expanded.len() != 3 {
        return fatal(
            interp,
            "sub-command GET_EXPERIMENTAL_FEATURE_ENABLED expects exactly two arguments",
        );
    }
    let feature_name = &expanded[1];
    let variable_name = &expanded[2];
    let Some((name, uuid, variable, description)) = EXPERIMENTAL_FEATURES
        .iter()
        .find(|(name, ..)| name == feature_name)
    else {
        return fatal(
            interp,
            format!("Experimental feature name \"{feature_name}\" does not exist."),
        );
    };
    let mut enabled = false;
    if let Some(value) = interp.get_def(variable).map(str::to_owned) {
        enabled = value == *uuid;
        if interp
            .ext
            .warned_experimental
            .insert(format!("{name}-{value}"))
        {
            if enabled {
                interp.issue(MessageType::AuthorWarning, description);
            } else {
                interp.issue(
                    MessageType::AuthorWarning,
                    &format!(
                        "{variable} is set to incorrect value\n  {value}\n\
                         See 'Help/dev/experimental.rst' in the source tree of \
                         this version of CMake for documentation of the \
                         experimental feature and the corresponding activation \
                         value.  This project's code may require changes to \
                         work with this CMake's version of the feature."
                    ),
                );
            }
        }
    }
    interp.set_def(
        variable_name.clone(),
        if enabled { "TRUE" } else { "FALSE" },
    );
    Ok(())
}
