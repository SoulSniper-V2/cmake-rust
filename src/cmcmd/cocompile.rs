//! `cmake -E __run_co_compile` (`cmcmd::HandleCoCompileCommands` with the
//! IWYU, clang-tidy, LWYU, cpplint, cppcheck and IAR C-STAT handlers) and
//! `cmake -E cmake_llvm_rc` (`cmcmd::RunLLVMRC`, `cmcmd::RunPreprocessor`).

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::commands::process::{exception_text, spawn_error_text};
use crate::util;

/// What `cmSystemTools::RunSingleCommand` captures with `OUTPUT_NONE`.
struct Captured {
    ok: bool,
    ret: i32,
    stdout: String,
    stderr: String,
}

/// `cmSystemTools::RunSingleCommand(cmd, &out, &err, &ret, nullptr,
/// OUTPUT_NONE)`; `merged` captures both streams into `stdout`.
fn run_single_command(command: &[String], merged: bool) -> Captured {
    let mut captured = Captured {
        ok: false,
        ret: 0,
        stdout: String::new(),
        stderr: String::new(),
    };
    let Some((program, args)) = command.split_first() else {
        return captured;
    };
    let mut cmd = Command::new(program);
    cmd.args(args).stdin(Stdio::inherit());
    let output = if merged {
        // The C++ merges both streams into one pipe.
        let (reader, writer) = match std::io::pipe() {
            Ok(pair) => pair,
            Err(error) => {
                captured.stdout = spawn_error_text(&error);
                return captured;
            }
        };
        let writer2 = match writer.try_clone() {
            Ok(writer2) => writer2,
            Err(error) => {
                captured.stdout = spawn_error_text(&error);
                return captured;
            }
        };
        cmd.stdout(writer).stderr(writer2);
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(error) => {
                captured.stdout = spawn_error_text(&error);
                return captured;
            }
        };
        drop(cmd);
        let mut data = Vec::new();
        let mut reader = reader;
        let _ = std::io::Read::read_to_end(&mut reader, &mut data);
        match child.wait() {
            Ok(status) => std::process::Output {
                status,
                stdout: data,
                stderr: Vec::new(),
            },
            Err(error) => {
                captured.stdout = spawn_error_text(&error);
                return captured;
            }
        }
    } else {
        match cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).output() {
            Ok(output) => output,
            Err(error) => {
                captured.stderr = spawn_error_text(&error);
                return captured;
            }
        }
    };
    captured.stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    captured.stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    match exception_text(output.status) {
        None => {
            captured.ok = true;
            captured.ret = output.status.code().unwrap_or(0);
        }
        Some(text) => {
            if merged {
                captured.stdout.push_str(&text);
            } else {
                captured.stderr.push_str(&text);
            }
        }
    }
    captured
}

/// `cmList{ runCmd, cmList::EmptyElements::Yes }`.
fn command_list(run: &str) -> Vec<String> {
    util::expand_list(run, true)
}

fn handle_iwyu(run: &str, _source: &str, orig: &[String]) -> i32 {
    let mut cmd = command_list(run);
    cmd.extend(orig.iter().skip(1).cloned());
    // Capture its stderr and hide its stdout.
    let result = run_single_command(&cmd, false);
    if !result.ok {
        eprintln!(
            "Error running '{}': {}",
            cmd.first().map_or("", String::as_str),
            result.stderr
        );
        return 1;
    }
    if result.stderr.contains("should remove these lines:")
        || result.stderr.contains("should add these lines:")
    {
        eprintln!(
            "Warning: include-what-you-use reported diagnostics:\n{}",
            result.stderr
        );
    }
    // Older versions of iwyu always returned a non-zero exit code, so
    // ignore it unless the user has enabled errors.
    let errors_enabled = cmd
        .iter()
        .position(|opt| opt.starts_with("--error"))
        .is_some_and(|index| index > 0 && cmd[index - 1] == "-Xiwyu");
    if errors_enabled { result.ret } else { 0 }
}

fn handle_tidy(run: &str, source: &str, orig: &[String]) -> i32 {
    let mut cmd = command_list(run);
    cmd.push(source.to_owned());
    for arg in &cmd {
        if let Some(file) = arg.strip_prefix("--export-fixes=") {
            let _ = std::fs::remove_file(file);
        }
    }
    // Prefer a compile_commands.json given by -p.
    if !cmd[..cmd.len() - 1].iter().any(|arg| arg == "-p") {
        cmd.push("--".to_owned());
        cmd.extend(orig.iter().cloned());
    }
    // Capture its stdout and hide its stderr.
    let result = run_single_command(&cmd, false);
    if !result.ok {
        eprintln!("Error running '{}': {}", cmd[0], result.stderr);
        return 1;
    }
    eprint!("{}", result.stdout);
    if result.ret != 0 {
        eprint!("{}", result.stderr);
    }
    result.ret
}

fn handle_lwyu(run: &str, source: &str, _orig: &[String]) -> i32 {
    let mut cmd = command_list(run);
    cmd.push(source.to_owned());
    let result = run_single_command(&cmd, false);
    if !result.ok {
        eprintln!("Error running '{}': {}", cmd[0], result.stderr);
        return 1;
    }
    if result.stdout.contains("Unused direct dependencies:") {
        eprint!("Warning: {}", result.stdout);
    }
    0
}

fn handle_cpplint(run: &str, source: &str, _orig: &[String]) -> i32 {
    let mut cmd = command_list(run);
    cmd.push(source.to_owned());
    let result = run_single_command(&cmd, true);
    if !result.ok {
        eprintln!("Error running '{}': {}", cmd[0], result.stdout);
        return 1;
    }
    if !result.stdout.is_empty() {
        eprint!("Warning: cpplint diagnostics:\n{}", result.stdout);
    }
    // cpplint returns non-zero for any warning.
    0
}

fn handle_cppcheck(run: &str, source: &str, orig: &[String]) -> i32 {
    let mut cmd = command_list(run);
    // Extract all the -D, -U, and -I options from the compile line.
    for opt in orig {
        let bytes = opt.as_bytes();
        if bytes.len() > 2 && bytes[0] == b'-' && matches!(bytes[1], b'D' | b'I' | b'U') {
            cmd.push(opt.clone());
        }
    }
    cmd.push(source.to_owned());
    let result = run_single_command(&cmd, false);
    if !result.ok {
        eprintln!("Error running '{}': {}", cmd[0], result.stdout);
        return 1;
    }
    eprint!("{}", result.stdout);
    let diagnostics = [
        "(error)",
        "(warning)",
        "(style)",
        "(performance)",
        "(portability)",
        "(information)",
    ];
    if diagnostics.iter().any(|kind| result.stderr.contains(kind)) {
        if result.ret == 0 {
            eprintln!("Warning: cppcheck reported diagnostics:");
        } else {
            eprintln!("Error: cppcheck reported failure:");
        }
    }
    eprint!("{}", result.stderr);
    result.ret
}

fn handle_icstat(run: &str, source: &str, orig: &[String]) -> i32 {
    let mut cmd = command_list(run);
    cmd.push("analyze".to_owned());
    cmd.push(source.to_owned());
    cmd.push("--".to_owned());
    cmd.extend(orig.iter().cloned());
    let mut stdout = String::new();
    let mut stderr = String::new();
    // Create the default manifest ruleset file when not found.
    if !Path::new("cstat_sel_checks.txt").exists() {
        let ichecks = format!(
            "{}/ichecks --default stdchecks",
            util::filename_path(orig.first().map_or("", String::as_str))
        );
        // RunSingleCommand(std::string) parses the command line.
        let parsed = crate::commands::try_compile::parse_arguments(&ichecks);
        let result = run_single_command(&parsed, false);
        stdout = result.stdout;
        stderr = result.stderr;
        if !result.ok {
            eprintln!("Error generating default manifest file '{ichecks}'. {stdout}");
            return 1;
        }
    }
    let result = run_single_command(&cmd, false);
    let _ = (&stdout, &stderr);
    stdout = result.stdout.clone();
    stderr = result.stderr.clone();
    if !result.ok {
        eprintln!("Error running '{}': {stdout}", cmd[0]);
        return 1;
    }
    if result.ret == 0 {
        eprintln!("Warning: C-STAT static analysis reported diagnostics:");
    } else {
        eprintln!("Error: C-STAT static analysis reported failure:");
    }
    eprint!("{stdout}");
    eprint!("{stderr}");
    result.ret
}

type Handler = fn(&str, &str, &[String]) -> i32;

/// The co-compiler table: option, handler, and whether the original
/// command is not run afterwards.
const CO_COMPILERS: [(&str, Handler, bool); 6] = [
    ("--cppcheck=", handle_cppcheck, false),
    ("--cpplint=", handle_cpplint, false),
    ("--icstat=", handle_icstat, false),
    ("--iwyu=", handle_iwyu, false),
    ("--lwyu=", handle_lwyu, true),
    ("--tidy=", handle_tidy, false),
];

/// `cmcmd::HandleCoCompileCommands` (`args` follow `__run_co_compile`).
pub fn run_co_compile(args: &[String]) -> i32 {
    let mut jobs: Vec<(String, Handler)> = Vec::new();
    let mut source = String::new();
    let mut launchers: Vec<String> = Vec::new();
    let mut run_original = true;
    let mut orig: Vec<String> = Vec::new();
    let mut doing_options = true;
    for arg in args {
        if arg == "--" {
            doing_options = false;
        } else if doing_options {
            let mut found = false;
            for (option, handler, no_original) in CO_COMPILERS {
                if let Some(command) = arg.strip_prefix(option) {
                    found = true;
                    jobs.push((command.to_owned(), handler));
                    if no_original {
                        run_original = false;
                    }
                }
            }
            if !found {
                if let Some(file) = arg.strip_prefix("--source=") {
                    source = file.to_owned();
                } else if let Some(launcher) = arg.strip_prefix("--launcher=") {
                    launchers.extend(util::expand_list(launcher, true));
                } else {
                    eprintln!("__run_co_compile given unknown argument: {arg}");
                    return 1;
                }
            }
        } else {
            orig.push(arg.clone());
        }
    }
    if jobs.is_empty() {
        eprintln!(
            "__run_co_compile missing command to run. Looking for one or more of the following:"
        );
        for (option, _, _) in CO_COMPILERS {
            eprintln!("{option}");
        }
        return 1;
    }
    if run_original && orig.is_empty() {
        eprintln!("__run_co_compile missing compile command after --");
        return 1;
    }
    for (command, handler) in &jobs {
        let ret = handler(command, &source, &orig);
        if ret != 0 {
            return ret;
        }
    }
    if !run_original {
        return 0;
    }
    let mut command = launchers;
    command.extend(orig);
    // OUTPUT_PASSTHROUGH
    match Command::new(&command[0]).args(&command[1..]).status() {
        Ok(status) => match exception_text(status) {
            None => status.code().unwrap_or(0),
            Some(text) => {
                eprintln!("{text}");
                eprintln!("Error running '{}'", command[0]);
                1
            }
        },
        Err(error) => {
            eprintln!("{}", spawn_error_text(&error));
            eprintln!("Error running '{}'", command[0]);
            1
        }
    }
}

/// `cmsys::RegularExpression::find` for the three fixed RunLLVMRC patterns:
/// `^[-/](alternatives)(.)?`, returning whether it matched and whether
/// group 2 was non-empty.
fn match_option(arg: &str, alternatives: &[&str]) -> Option<bool> {
    let rest = arg.strip_prefix(['-', '/'])?;
    // Alternatives are tried left to right, like the regex.
    let after = alternatives.iter().find_map(|alt| rest.strip_prefix(alt))?;
    Some(!after.is_empty())
}

/// `cmcmd::RunPreprocessor`.
fn run_preprocessor(command: &[String], intermediate: &str) -> i32 {
    // uv_fs_open(O_CREAT | O_RDWR, 0644): no truncation.
    let output = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o644)
        .open(intermediate);
    let mut cmd = Command::new(&command[0]);
    cmd.args(&command[1..]).stderr(Stdio::piped());
    match output {
        Ok(file) => {
            cmd.stdout(file);
        }
        Err(_) => {
            cmd.stdout(Stdio::null());
        }
    }
    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(_) => {
            eprint!("Failed to start preprocessor.");
            return 1;
        }
    };
    let output = match child.wait_with_output() {
        Ok(output) => output,
        Err(_) => {
            eprint!("Failed to wait for preprocessor");
            return 1;
        }
    };
    if output.status.code() != Some(0) {
        let _ = std::io::stderr().write_all(&output.stderr);
        return 1;
    }
    0
}

/// `cmcmd::RunLLVMRC` (`args` follow `cmake_llvm_rc`).
pub fn run_llvm_rc(args: &[String]) -> i32 {
    if args.is_empty() {
        eprint!("Invalid cmake_llvm_rc arguments");
        return 1;
    }
    let source_file = args[0].as_str();
    let intermediate = args.get(1).map_or("", String::as_str);
    let mut preprocess: Vec<String> = Vec::new();
    let mut resource_compile: Vec<String> = Vec::new();
    let mut to_rc = false;
    let mut accept_next = false;
    let mut skip_next = false;
    let source_dir = util::filename_path(source_file);
    for arg in args.iter().skip(2) {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "++" {
            to_rc = true;
            skip_next = false;
            accept_next = true;
            continue;
        }
        if !accept_next {
            if let Some(has_value) = match_option(arg, &["D", "U", "I", "FO", "fo", "Fo"]) {
                accept_next = !has_value;
            } else if match_option(arg, &["N", "Y"]).is_some() {
                if !to_rc {
                    continue;
                }
            } else if let Some(has_value) = match_option(arg, &["C", "LN", "L"]) {
                if !to_rc {
                    skip_next = !has_value;
                    continue;
                }
                accept_next = !has_value;
            } else if to_rc {
                continue;
            }
        } else {
            accept_next = false;
        }
        let value = if arg.contains("SOURCE_DIR") {
            arg.replace("SOURCE_DIR", &source_dir)
        } else {
            arg.clone()
        };
        if to_rc {
            resource_compile.push(value);
        } else {
            preprocess.push(value);
        }
    }
    if preprocess.is_empty() {
        eprint!("Empty preprocessing command");
        return 1;
    }
    if resource_compile.is_empty() {
        eprint!("Empty resource compilation command");
        return 1;
    }
    // The llvm-rc source file may have been skipped above.
    if resource_compile.last().map(String::as_str) != Some(intermediate) {
        resource_compile.push(intermediate.to_owned());
    }
    let result = run_preprocessor(&preprocess, intermediate);
    if result != 0 {
        let _ = std::fs::remove_file(intermediate);
        return result;
    }
    let mut cmd = Command::new(&resource_compile[0]);
    cmd.args(&resource_compile[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let outcome = match cmd.spawn() {
        Err(_) => {
            eprint!("Failed to start resource compiler.");
            Err(1)
        }
        Ok(child) => match child.wait_with_output() {
            Ok(output) => Ok(output),
            Err(_) => {
                eprint!("Failed to wait for resource compiler");
                Err(1)
            }
        },
    };
    let _ = std::fs::remove_file(intermediate);
    let output = match outcome {
        Ok(output) => output,
        Err(code) => return code,
    };
    if output.status.code() != Some(0) {
        let _ = std::io::stderr().write_all(&output.stderr);
        return 1;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::match_option;

    #[test]
    fn llvm_rc_option_patterns() {
        assert_eq!(
            match_option("/FO", &["D", "U", "I", "FO", "fo", "Fo"]),
            Some(false)
        );
        assert_eq!(
            match_option("-DX=1", &["D", "U", "I", "FO", "fo", "Fo"]),
            Some(true)
        );
        assert_eq!(
            match_option("-bad", &["D", "U", "I", "FO", "fo", "Fo"]),
            None
        );
        assert_eq!(match_option("/LN", &["C", "LN", "L"]), Some(false));
        assert_eq!(match_option("/L0x409", &["C", "LN", "L"]), Some(true));
    }
}
