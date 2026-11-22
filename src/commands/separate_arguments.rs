//! `separate_arguments()` command: port of `cmSeparateArgumentsCommand.cxx`,
//! `cmSystemTools::ParseUnixCommandLine` (kwsysSystem__ParseUnixCommand),
//! `ParseWindowsCommandLine`, `SplitProgramFromArgs` and `FindProgram`.

use crate::interp::{CmdResult, Interp, err};
use crate::util;

pub fn register(interp: &mut Interp) {
    interp.register("separate_arguments", separate_arguments_command);
}

fn is_space(c: u8) -> bool {
    c.is_ascii_whitespace()
}

/// `kwsysSystem__ParseUnixCommand`.
fn parse_unix_command_line(command: &str) -> Vec<String> {
    let bytes = command.as_bytes();
    let mut args = Vec::new();
    let mut arg = String::new();
    let mut in_argument = false;
    let mut in_escape = false;
    let mut in_single = false;
    let mut in_double = false;
    for &c in bytes {
        if in_escape {
            in_argument = true;
            arg.push(c as char);
            in_escape = false;
        } else if c == b'\\' {
            in_escape = true;
        } else if c == b'\'' && !in_double {
            if in_single {
                in_single = false;
            } else {
                in_single = true;
                in_argument = true;
            }
        } else if c == b'"' && !in_single {
            if in_double {
                in_double = false;
            } else {
                in_double = true;
                in_argument = true;
            }
        } else if is_space(c) {
            if in_argument {
                if in_single || in_double {
                    arg.push(c as char);
                } else {
                    args.push(std::mem::take(&mut arg));
                    in_argument = false;
                }
            }
        } else {
            in_argument = true;
            arg.push(c as char);
        }
    }
    if in_argument {
        args.push(arg);
    }
    args
}

/// `cmSystemTools::ParseWindowsCommandLine`.
fn parse_windows_command_line(command: &str) -> Vec<String> {
    let bytes = command.as_bytes();
    let mut args = Vec::new();
    let mut arg = String::new();
    let mut in_argument = false;
    let mut in_quotes = false;
    let mut backslashes = 0usize;
    for &c in bytes {
        if c == b'\\' {
            backslashes += 1;
            in_argument = true;
        } else if c == b'"' {
            let backslash_pairs = backslashes >> 1;
            let backslash_escaped = backslashes & 1;
            arg.extend(std::iter::repeat_n('\\', backslash_pairs));
            backslashes = 0;
            if backslash_escaped != 0 {
                arg.push('"');
            } else {
                in_quotes = !in_quotes;
            }
            in_argument = true;
        } else {
            arg.extend(std::iter::repeat_n('\\', backslashes));
            backslashes = 0;
            if is_space(c) {
                if in_quotes {
                    arg.push(c as char);
                } else if in_argument {
                    args.push(std::mem::take(&mut arg));
                    in_argument = false;
                }
            } else {
                in_argument = true;
                arg.push(c as char);
            }
        }
    }
    arg.extend(std::iter::repeat_n('\\', backslashes));
    if in_argument {
        args.push(arg);
    }
    args
}

/// `cmSystemTools::SplitProgramFromArgs` — returns (program, args).
fn split_program_from_args(command: &str) -> (String, String, bool) {
    let bytes = command.as_bytes();
    let mut c = 0usize;
    while c < bytes.len() && is_space(bytes[c]) {
        c += 1;
    }
    let mut program = String::new();
    let mut in_escape = false;
    let mut in_double = false;
    let mut in_single = false;
    while c < bytes.len() {
        let ch = bytes[c];
        if in_single {
            if ch == b'\'' {
                in_single = false;
            } else {
                program.push(ch as char);
            }
        } else if in_escape {
            in_escape = false;
            program.push(ch as char);
        } else if ch == b'\\' {
            in_escape = true;
        } else if in_double {
            if ch == b'"' {
                in_double = false;
            } else {
                program.push(ch as char);
            }
        } else if ch == b'"' {
            in_double = true;
        } else if ch == b'\'' {
            in_single = true;
        } else if is_space(ch) {
            break;
        } else {
            program.push(ch as char);
        }
        c += 1;
    }
    let args = command[c..].to_owned();
    let ok = !in_single && !in_escape && !in_double;
    (program, args, ok)
}

/// `SystemTools::FileIsExecutable` (POSIX).
fn file_is_executable(path: &str) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(std::ffi::OsStr::new(path).as_bytes());
    let Ok(c) = c else { return false };
    // !FileIsDirectory && access(X_OK)
    let is_dir = std::fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false);
    !is_dir && unsafe { libc::access(c.as_ptr(), libc::X_OK) == 0 }
}

/// `SystemTools::FileExists` (POSIX): `access(name, R_OK)`.
fn file_exists(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(std::ffi::OsStr::new(path).as_bytes());
    let Ok(c) = c else { return false };
    unsafe { libc::access(c.as_ptr(), libc::R_OK) == 0 }
}

/// `cmSystemTools::FindProgram` (POSIX subset).
fn find_program(name: &str) -> String {
    if file_is_executable(name) {
        return util::collapse_full_path(name, "");
    }
    let path_env = std::env::var("PATH").unwrap_or_default();
    for dir in path_env.split(':') {
        let mut p = dir.to_owned();
        if p.is_empty() || !p.ends_with('/') {
            p.push('/');
        }
        p.push_str(name);
        if file_is_executable(&p) {
            return util::collapse_full_path(&p, "");
        }
    }
    String::new()
}

fn trim_whitespace(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_ascii_whitespace())
}

fn separate_arguments_command(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.is_empty() {
        return err("must be given at least one argument.");
    }
    let var = args[0].clone();
    if args.len() == 1 {
        // Original space-replacement version of command.
        if let Some(def) = interp.get_def(&var) {
            let value: String = def
                .chars()
                .map(|c| if c == ' ' { ';' } else { c })
                .collect();
            interp.set_def(var, value);
        }
        return Ok(());
    }

    // cmArgumentParser flag binding.
    let mut unix_command = false;
    let mut windows_command = false;
    let mut native_command = false;
    let mut program = false;
    let mut separate_args = false;
    let mut unparsed: Vec<String> = Vec::new();
    for arg in &args[1..] {
        match arg.as_str() {
            "UNIX_COMMAND" => unix_command = true,
            "WINDOWS_COMMAND" => windows_command = true,
            "NATIVE_COMMAND" => native_command = true,
            "PROGRAM" => program = true,
            "SEPARATE_ARGS" => separate_args = true,
            _ => unparsed.push(arg.clone()),
        }
    }

    if !unix_command && !windows_command && !native_command {
        return err(
            "missing required option: 'UNIX_COMMAND' or 'WINDOWS_COMMAND' or 'NATIVE_COMMAND'",
        );
    }
    if (unix_command && (windows_command || native_command)) || (windows_command && native_command)
    {
        return err(
            "'UNIX_COMMAND', 'WINDOWS_COMMAND' and 'NATIVE_COMMAND' are mutually exclusive",
        );
    }
    if separate_args && !program {
        return err("`SEPARATE_ARGS` option requires `PROGRAM' option");
    }
    if unparsed.len() > 1 {
        return err("given unexpected argument(s)");
    }
    if unparsed.is_empty() {
        interp.set_def(var, "");
        return Ok(());
    }
    let command = unparsed.into_iter().next().unwrap();
    if command.is_empty() {
        interp.set_def(var, "");
        return Ok(());
    }

    if program && !separate_args {
        let mut prog = String::new();
        let mut program_args = String::new();
        if !trim_whitespace(&command).is_empty() {
            prog = find_program(&command);
        }
        if prog.is_empty() {
            let (split, rest, ok) = split_program_from_args(&command);
            prog = split;
            program_args = rest;
            if ok && !file_exists(&prog) {
                prog = find_program(&prog);
            }
        }
        if !prog.is_empty() {
            prog.push(';');
            prog.push_str(&program_args);
        }
        interp.set_def(var, prog);
        return Ok(());
    }

    // split command given
    if native_command {
        unix_command = true;
    }
    let mut values = if unix_command {
        parse_unix_command_line(&command)
    } else {
        parse_windows_command_line(&command)
    };

    if program {
        // check program exists
        let first = values.first().cloned().unwrap_or_default();
        if !file_exists(&first) {
            let result = find_program(&first);
            if result.is_empty() {
                values.clear();
            } else if let Some(first) = values.first_mut() {
                *first = result;
            }
        }
    }

    // preserve semicolons in arguments
    for value in &mut values {
        *value = value.replace(';', "\\;");
    }
    interp.set_def(var, util::join_list(&values));
    Ok(())
}
