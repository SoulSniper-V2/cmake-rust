use std::fs::File;
use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use super::Interpreter;
use crate::{Result, fail};

const OPTIONS: &[&str] = &[
    "COMMAND",
    "WORKING_DIRECTORY",
    "TIMEOUT",
    "RESULT_VARIABLE",
    "RESULTS_VARIABLE",
    "OUTPUT_VARIABLE",
    "ERROR_VARIABLE",
    "INPUT_FILE",
    "OUTPUT_FILE",
    "ERROR_FILE",
    "OUTPUT_QUIET",
    "ERROR_QUIET",
    "OUTPUT_STRIP_TRAILING_WHITESPACE",
    "ERROR_STRIP_TRAILING_WHITESPACE",
    "COMMAND_ECHO",
    "COMMAND_ERROR_IS_FATAL",
    "ECHO_OUTPUT_VARIABLE",
    "ECHO_ERROR_VARIABLE",
    "ENCODING",
];

impl Interpreter {
    pub(super) fn execute_process(&mut self, args: &[String]) -> Result<()> {
        let mut command = None;
        let mut working_directory = None;
        let mut result = None;
        let mut results = None;
        let mut input_file = None;
        let mut output = OutputOptions::default();
        let mut error = OutputOptions::default();
        let mut fatal = false;
        let mut echo = None;
        let mut rest = args;
        while let Some((keyword, tail)) = rest.split_first() {
            let length = tail
                .iter()
                .position(|arg| OPTIONS.contains(&arg.as_str()))
                .unwrap_or(tail.len());
            let values = &tail[..length];
            rest = &tail[length..];
            match (keyword.as_str(), values) {
                ("COMMAND", values) if command.is_none() && !values.is_empty() => {
                    command = Some(values)
                }
                ("WORKING_DIRECTORY", [path]) => working_directory = Some(path),
                ("RESULT_VARIABLE", [name]) => result = Some(name),
                ("RESULTS_VARIABLE", [name]) => results = Some(name),
                ("INPUT_FILE", [path]) => input_file = Some(path),
                ("OUTPUT_VARIABLE", [name]) => output.variable = Some(name),
                ("ERROR_VARIABLE", [name]) => error.variable = Some(name),
                ("OUTPUT_FILE", [path]) => output.file = Some(path),
                ("ERROR_FILE", [path]) => error.file = Some(path),
                ("OUTPUT_QUIET", []) => output.quiet = true,
                ("ERROR_QUIET", []) => error.quiet = true,
                ("OUTPUT_STRIP_TRAILING_WHITESPACE", []) => output.trim = true,
                ("ERROR_STRIP_TRAILING_WHITESPACE", []) => error.trim = true,
                ("ECHO_OUTPUT_VARIABLE", []) => output.echo = true,
                ("ECHO_ERROR_VARIABLE", []) => error.echo = true,
                ("COMMAND_ERROR_IS_FATAL", [mode]) if matches!(mode.as_str(), "ANY" | "LAST") => {
                    fatal = true
                }
                ("COMMAND_ECHO", [mode])
                    if matches!(mode.as_str(), "STDOUT" | "STDERR" | "NONE") =>
                {
                    echo = Some(mode.clone())
                }
                _ => {
                    return fail(format!(
                        "unsupported execute_process arguments {keyword} {values:?}"
                    ));
                }
            }
        }
        let command = command.ok_or("execute_process needs a COMMAND")?;
        if output.variable.is_some()
            && output.variable == error.variable
            && !output.quiet
            && !error.quiet
        {
            return fail("execute_process requires separate output and error variables");
        }
        if output.file.is_some() && output.file == error.file {
            return fail("execute_process requires separate output and error files");
        }

        let mut process = Command::new(&command[0]);
        process.args(&command[1..]);
        if let Some(path) = working_directory {
            process.current_dir(path);
        }
        if let Some(path) = input_file {
            process.stdin(File::open(path)?);
        } else {
            process.stdin(Stdio::inherit());
        }
        process.stdout(output.stdio()?).stderr(error.stdio()?);
        let echo = echo.or_else(|| self.get("CMAKE_EXECUTE_PROCESS_COMMAND_ECHO"));
        match echo.as_deref() {
            Some("STDOUT") => println!("{process:?}"),
            Some("STDERR") => eprintln!("{process:?}"),
            Some("NONE") | None => {}
            Some(mode) => {
                return fail(format!("unsupported execute_process COMMAND_ECHO {mode:?}"));
            }
        }
        let (status, succeeded, stdout, stderr) = match process.output() {
            Ok(output) => (
                exit_result(output.status),
                output.status.success(),
                output.stdout,
                output.stderr,
            ),
            Err(error) => (error.to_string(), false, Vec::new(), Vec::new()),
        };
        if output.echo && !output.quiet {
            io::stdout().write_all(&stdout)?;
        }
        if error.echo && !error.quiet {
            io::stderr().write_all(&stderr)?;
        }
        output.store(self, &stdout);
        error.store(self, &stderr);
        for variable in [result, results].into_iter().flatten() {
            self.set(variable, &status);
        }
        if fatal && !succeeded {
            return fail(format!(
                "command {:?} failed with result {status}",
                command[0]
            ));
        }
        Ok(())
    }
}

#[derive(Default)]
struct OutputOptions<'a> {
    variable: Option<&'a String>,
    file: Option<&'a String>,
    quiet: bool,
    trim: bool,
    echo: bool,
}

impl OutputOptions<'_> {
    fn stdio(&self) -> Result<Stdio> {
        Ok(if let Some(path) = self.file {
            Stdio::from(File::create(Path::new(path))?)
        } else if self.quiet {
            Stdio::null()
        } else if self.variable.is_some() {
            Stdio::piped()
        } else {
            Stdio::inherit()
        })
    }

    fn store(&self, interpreter: &mut Interpreter, bytes: &[u8]) {
        if let Some(variable) = self.variable.filter(|_| !self.quiet) {
            let value = String::from_utf8_lossy(bytes).replace("\r\n", "\n");
            interpreter.set(variable, if self.trim { value.trim_end() } else { &value });
        }
    }
}

pub(super) fn exit_result(status: std::process::ExitStatus) -> String {
    status
        .code()
        .map_or_else(|| status.to_string(), |code| code.to_string())
}
