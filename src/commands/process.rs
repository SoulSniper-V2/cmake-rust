//! execute_process: concurrent pipeline setup, stream routing and status collection.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use crate::interp::{CmdResult, Interp, err};
use crate::messages::MessageType;
use crate::util::{collapse_full_path, is_subdirectory};

pub fn register(interp: &mut Interp) {
    interp.register("execute_process", execute_process);
}

const VALUE_OPTIONS: &[&str] = &[
    "COMMAND_ECHO",
    "OUTPUT_VARIABLE",
    "ERROR_VARIABLE",
    "RESULT_VARIABLE",
    "RESULTS_VARIABLE",
    "WORKING_DIRECTORY",
    "INPUT_FILE",
    "OUTPUT_FILE",
    "ERROR_FILE",
    "TIMEOUT",
    "ENCODING",
    "COMMAND_ERROR_IS_FATAL",
];
const FLAGS: &[&str] = &[
    "OUTPUT_QUIET",
    "ERROR_QUIET",
    "OUTPUT_STRIP_TRAILING_WHITESPACE",
    "ERROR_STRIP_TRAILING_WHITESPACE",
    "ECHO_OUTPUT_VARIABLE",
    "ECHO_ERROR_VARIABLE",
];
const TIMEOUT: &str = "Process terminated due to timeout";

#[derive(Default)]
struct Options<'a> {
    values: HashMap<&'a str, &'a str>,
    flags: HashSet<&'a str>,
    commands: Vec<&'a [String]>,
}

impl<'a> Options<'a> {
    fn get(&self, name: &str) -> &'a str {
        self.values.get(name).copied().unwrap_or_default()
    }
}

fn keyword(arg: &str) -> bool {
    arg == "COMMAND" || VALUE_OPTIONS.contains(&arg) || FLAGS.contains(&arg)
}

fn parse<'a>(
    interp: &mut Interp,
    args: &'a [String],
) -> Result<Option<Options<'a>>, crate::interp::CmdErr> {
    if args.is_empty() {
        return err("called with incorrect number of arguments");
    }

    let mut options = Options::default();
    let mut missing = std::collections::BTreeMap::<&str, usize>::new();
    let mut unknown = None;
    let mut index = 0;
    while let Some(arg) = args.get(index).map(String::as_str) {
        index += 1;
        if arg == "COMMAND" {
            let start = index;
            while index < args.len() && !keyword(&args[index]) {
                index += 1;
            }
            options.commands.push(&args[start..index]);
        } else if VALUE_OPTIONS.contains(&arg) {
            if let Some(value) = args.get(index).filter(|value| !keyword(value)) {
                options.values.insert(arg, value);
                index += 1;
            } else {
                *missing.entry(arg).or_default() += 1;
            }
        } else if FLAGS.contains(&arg) {
            options.flags.insert(arg);
        } else if unknown.is_none() {
            unknown = Some(arg);
        }
    }
    if !missing.is_empty() {
        let mut text = String::new();
        for (name, count) in missing {
            text.push_str(&format!("Error after keyword \"{name}\":\n"));
            for _ in 0..count {
                text.push_str("  missing required value\n");
            }
        }
        interp.issue(MessageType::FatalError, &text);
        return Ok(None);
    }
    if let Some(arg) = unknown {
        return err(format!(" given unknown argument \"{arg}\"."));
    }
    Ok(Some(options))
}

fn pipe() -> io::Result<(File, File)> {
    let mut fds = [0; 2];
    // Each returned descriptor is owned by exactly one File. CLOEXEC avoids
    // leaking pipeline endpoints to unrelated stages and their descendants.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let files = unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) };
    for fd in fds {
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(files)
}

#[derive(Debug)]
enum Status {
    Exit(i32),
    Abnormal(String),
}

impl Status {
    fn result(&self) -> String {
        match self {
            Self::Exit(code) => code.to_string(),
            Self::Abnormal(message) => message.clone(),
        }
    }

    fn failure(&self) -> Option<String> {
        match self {
            Self::Exit(0) => None,
            Self::Exit(code) => Some(format!("Child return code: {code}")),
            Self::Abnormal(message) => {
                Some(format!("Abnormal exit with child return code: {message}"))
            }
        }
    }
}

impl From<ExitStatus> for Status {
    fn from(status: ExitStatus) -> Self {
        if let Some(code) = status.code() {
            return Self::Exit(code);
        }
        Self::Abnormal(
            match status.signal() {
                Some(libc::SIGSEGV) => "Segmentation fault",
                Some(libc::SIGBUS) => "Bus error",
                Some(libc::SIGFPE) => "Floating-point exception",
                Some(libc::SIGILL) => "Illegal instruction",
                Some(libc::SIGINT) => "User interrupt",
                Some(libc::SIGABRT) => "Subprocess aborted",
                Some(libc::SIGKILL) => "Subprocess killed",
                Some(libc::SIGTERM) => "Subprocess terminated",
                Some(libc::SIGHUP) => "SIGHUP",
                Some(libc::SIGQUIT) => "SIGQUIT",
                Some(libc::SIGTRAP) => "SIGTRAP",
                Some(libc::SIGUSR1) => "SIGUSR1",
                Some(libc::SIGUSR2) => "SIGUSR2",
                Some(libc::SIGPIPE) => "SIGPIPE",
                Some(libc::SIGALRM) => "SIGALRM",
                Some(libc::SIGCHLD) => "SIGCHLD",
                Some(libc::SIGCONT) => "SIGCONT",
                Some(libc::SIGSTOP) => "SIGSTOP",
                Some(libc::SIGTSTP) => "SIGTSTP",
                Some(libc::SIGTTIN) => "SIGTTIN",
                Some(libc::SIGTTOU) => "SIGTTOU",
                Some(libc::SIGURG) => "SIGURG",
                Some(libc::SIGXCPU) => "SIGXCPU",
                Some(libc::SIGXFSZ) => "SIGXFSZ",
                Some(libc::SIGVTALRM) => "SIGVTALRM",
                Some(libc::SIGPROF) => "SIGPROF",
                Some(libc::SIGWINCH) => "SIGWINCH",
                Some(libc::SIGSYS) => "SIGSYS",
                #[cfg(target_os = "linux")]
                Some(libc::SIGPOLL) => "SIGPOLL",
                #[cfg(target_os = "linux")]
                Some(libc::SIGPWR) => "SIGPWR",
                #[cfg(target_os = "linux")]
                Some(libc::SIGSTKFLT) => "SIGSTKFLT",
                _ => return Self::Abnormal(format!("Signal {}", status.signal().unwrap_or(0))),
            }
            .into(),
        )
    }
}

/// The `cmUVProcessChain` exception text of an abnormal exit (a signal),
/// or `None` for a normal exit.
pub(crate) fn exception_text(status: ExitStatus) -> Option<String> {
    match Status::from(status) {
        Status::Exit(_) => None,
        Status::Abnormal(message) => Some(message),
    }
}

/// The libuv text of a spawn failure (`uv_strerror`, lower case).
pub(crate) fn spawn_error_text(error: &io::Error) -> String {
    match spawn_error_status(error) {
        Status::Abnormal(message) => message,
        Status::Exit(_) => String::new(),
    }
}

fn spawn_error_status(error: &io::Error) -> Status {
    let text = error.to_string();
    let message = text
        .split_once(" (os error ")
        .map_or(text.as_str(), |(message, _)| message);
    Status::Abnormal(message.to_ascii_lowercase())
}

fn spawn_error(error: io::Error) -> Status {
    let text = error.to_string();
    let message = text
        .split_once(" (os error ")
        .map_or(text.as_str(), |(message, _)| message);
    Status::Abnormal(message.to_ascii_lowercase())
}

enum Stage {
    Running(Child),
    Complete(Status),
}

struct Pipeline(Vec<Stage>);

impl Drop for Pipeline {
    fn drop(&mut self) {
        let children: Vec<_> = self
            .0
            .drain(..)
            .filter_map(|stage| match stage {
                Stage::Running(child) => Some(child),
                Stage::Complete(_) => None,
            })
            .collect();
        if !children.is_empty() {
            // The deadline ends capture. Kitware does not signal processes
            // that were still running when the capture deadline expired.
            let _ = std::thread::Builder::new().spawn(move || {
                for mut child in children {
                    let _ = child.wait();
                }
            });
        }
    }
}

#[derive(Default)]
struct Capture {
    reader: Option<File>,
    bytes: Vec<u8>,
    quiet: bool,
    capture: bool,
    echo: bool,
    strip: bool,
}

impl Capture {
    fn output(&mut self, data: &[u8], stderr: bool) {
        if self.quiet {
            return;
        }
        if self.echo || !self.capture {
            if stderr {
                let _ = io::stderr().write_all(data);
            } else {
                let mut stdout = io::stdout().lock();
                let _ = stdout.write_all(data);
                let _ = stdout.flush();
            }
        }
        if self.capture {
            self.bytes.extend_from_slice(data);
        }
    }

    fn text(&self) -> String {
        let mut result = Vec::with_capacity(self.bytes.len());
        for (index, byte) in self.bytes.iter().copied().enumerate() {
            if byte != 0 && !(byte == b'\r' && self.bytes.get(index + 1) == Some(&b'\n')) {
                result.push(byte);
            }
        }
        if self.strip {
            while result
                .last()
                .is_some_and(|byte| matches!(byte, b' ' | b'\r' | b'\n' | b'\t' | 11 | 12))
            {
                result.pop();
            }
        }
        crate::bytes::decode(&result)
    }
}

fn stream(capture: &mut Capture, filename: &str) -> io::Result<Option<File>> {
    if !filename.is_empty() {
        return Ok(File::create(crate::bytes::path(filename)).ok());
    }
    let (reader, writer) = pipe()?;
    capture.reader = Some(reader);
    Ok(Some(writer))
}

fn duplicate(file: Option<&File>) -> io::Result<Stdio> {
    file.map_or_else(
        || Ok(Stdio::null()),
        |file| file.try_clone().map(Stdio::from),
    )
}

fn filename(options: &Options<'_>, option: &str) -> String {
    let path = options.get(option);
    let cwd = options.get("WORKING_DIRECTORY");
    if path.is_empty() || cwd.is_empty() {
        path.to_owned()
    } else {
        collapse_full_path(path, &collapse_full_path(cwd, ""))
    }
}

fn can_write_output(interp: &Interp, path: &str) -> bool {
    if !interp.is_on("CMAKE_DISABLE_SOURCE_CHANGES") {
        return true;
    }
    let source = std::fs::canonicalize(&interp.home_source).ok();
    let binary = std::fs::canonicalize(&interp.home_binary).ok();
    if interp.home_source == interp.home_binary || source.is_some() && source == binary {
        return !interp.is_on("CMAKE_DISABLE_IN_SOURCE_BUILD");
    }
    !is_subdirectory(path, &interp.home_source)
        || is_subdirectory(path, &interp.home_binary)
        || path == interp.home_binary
}

fn run_pipeline(
    options: &Options<'_>,
    captures: &mut [Capture; 2],
    timeout: Option<Duration>,
) -> io::Result<Option<Vec<Status>>> {
    let cwd = options.get("WORKING_DIRECTORY");
    let input = filename(options, "INPUT_FILE");
    let output = filename(options, "OUTPUT_FILE");
    let error = filename(options, "ERROR_FILE");
    let merged = !options.get("OUTPUT_VARIABLE").is_empty()
        && options.get("OUTPUT_VARIABLE") == options.get("ERROR_VARIABLE");
    let out_pipe = stream(&mut captures[0], &output)?;
    let err_pipe = if (!error.is_empty() && error == output)
        || (error.is_empty() && merged && output.is_empty())
    {
        out_pipe.as_ref().map(File::try_clone).transpose()?
    } else if error.is_empty() && merged {
        None
    } else {
        stream(&mut captures[1], &error)?
    };

    let mut previous = None;
    let mut stages = Pipeline(Vec::with_capacity(options.commands.len()));
    for (index, args) in options.commands.iter().enumerate() {
        let last = index + 1 == options.commands.len();
        let mut cmd = Command::new(crate::bytes::encode_os(&args[0]));
        cmd.args(args[1..].iter().map(|arg| crate::bytes::encode_os(arg)));
        if !cwd.is_empty() {
            cmd.current_dir(crate::bytes::path(cwd));
        }
        if index == 0 {
            cmd.stdin(if input.is_empty() {
                Stdio::inherit()
            } else {
                duplicate(File::open(crate::bytes::path(&input)).ok().as_ref())?
            });
        } else {
            cmd.stdin(duplicate(previous.as_ref())?);
        }
        let next = if last {
            cmd.stdout(duplicate(out_pipe.as_ref())?);
            None
        } else {
            let (reader, writer) = pipe()?;
            cmd.stdout(writer);
            Some(reader)
        };
        cmd.stderr(duplicate(err_pipe.as_ref())?);
        stages.0.push(match cmd.spawn() {
            Ok(child) => Stage::Running(child),
            Err(error) => Stage::Complete(spawn_error(error)),
        });
        previous = next;
    }
    drop((out_pipe, err_pipe));

    let started = Instant::now();
    let mut data = [0; 16384];
    loop {
        if timeout.is_some_and(|duration| started.elapsed() >= duration) {
            return Ok(None);
        }
        let mut finished = true;
        for stage in &mut stages.0 {
            if let Stage::Running(child) = stage {
                if let Some(status) = child.try_wait()? {
                    *stage = Stage::Complete(status.into());
                } else {
                    finished = false;
                }
            }
        }
        if finished && captures.iter().all(|stream| stream.reader.is_none()) {
            return Ok(Some(
                stages
                    .0
                    .drain(..)
                    .filter_map(|stage| match stage {
                        Stage::Complete(status) => Some(status),
                        Stage::Running(_) => None,
                    })
                    .collect(),
            ));
        }

        let mut pollfds: Vec<_> = captures
            .iter()
            .map(|capture| libc::pollfd {
                fd: capture.reader.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        let delay = timeout.map_or(10, |limit| {
            limit.saturating_sub(started.elapsed()).as_millis().min(10) as i32
        });
        if unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as libc::nfds_t, delay) } < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
            continue;
        }
        for (index, (fd, capture)) in pollfds.iter().zip(captures.iter_mut()).enumerate() {
            if fd.revents == 0 {
                continue;
            }
            if let Some(reader) = &mut capture.reader {
                match reader.read(&mut data) {
                    Ok(0) => capture.reader = None,
                    Ok(count) => capture.output(&data[..count], index == 1),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => return Err(error),
                }
            }
        }
    }
}

fn execute_process(interp: &mut Interp, args: &[String]) -> CmdResult {
    let Some(options) = parse(interp, args)? else {
        return Ok(());
    };
    let output = filename(&options, "OUTPUT_FILE");
    if !can_write_output(interp, &output) {
        interp.set_fatal();
        return err(format!(
            "attempted to output into a file: {output} into a source directory."
        ));
    }
    if options.commands.is_empty() {
        return err(" called with no COMMAND argument.");
    }
    if options.commands.iter().any(|cmd| cmd.is_empty()) {
        return err(" given COMMAND argument with no value.");
    }
    let mut seconds = -1.0_f64;
    if !options.get("TIMEOUT").is_empty() {
        let value = std::ffi::CString::new(options.get("TIMEOUT"))
            .map_err(|_| " called with TIMEOUT value that could not be parsed.")?;
        if unsafe { libc::sscanf(value.as_ptr(), c"%lg".as_ptr(), &mut seconds) } != 1 {
            return err(" called with TIMEOUT value that could not be parsed.");
        }
    }
    let millis = (seconds * 1000.0).trunc();
    let timeout =
        (millis >= 0.0 && millis < i64::MAX as f64).then(|| Duration::from_millis(millis as u64));

    let mut fatal = options.get("COMMAND_ERROR_IS_FATAL").to_owned();
    if fatal.is_empty()
        && options.get("RESULT_VARIABLE").is_empty()
        && options.get("RESULTS_VARIABLE").is_empty()
    {
        fatal = interp.get_safe("CMAKE_EXECUTE_PROCESS_COMMAND_ERROR_IS_FATAL");
    }
    if !matches!(fatal.as_str(), "" | "ANY" | "LAST" | "NONE") {
        if options.get("COMMAND_ERROR_IS_FATAL").is_empty() {
            return err(format!(
                "Using CMAKE_EXECUTE_PROCESS_COMMAND_ERROR_IS_FATAL with invalid value \"{fatal}\". This variable can be ANY, LAST or NONE"
            ));
        }
        return err("COMMAND_ERROR_IS_FATAL option can be ANY, LAST or NONE");
    }

    let explicit_echo = options.get("COMMAND_ECHO");
    let echo = if explicit_echo.is_empty() {
        interp.get_safe("CMAKE_EXECUTE_PROCESS_COMMAND_ECHO")
    } else {
        explicit_echo.to_owned()
    };
    match echo.as_str() {
        "STDOUT" | "STDERR" => {
            for command in &options.commands {
                let line = format!("'{}'", command.join("' '"));
                if echo == "STDOUT" {
                    interp.stdout_line(&line);
                } else {
                    crate::bytes::stderr_line(&line);
                }
            }
        }
        "" | "NONE" => {}
        _ => {
            let message = if explicit_echo.is_empty() {
                format!(
                    "CMAKE_EXECUTE_PROCESS_COMMAND_ECHO set to '{echo}' expected STDERR|STDOUT|NONE"
                )
            } else {
                format!(" called with '{echo}' expected STDERR|STDOUT|NONE for COMMAND_ECHO.")
            };
            interp.issue(MessageType::FatalError, &message);
            return Ok(());
        }
    }
    if let Some(encoding) = options.values.get("ENCODING")
        && !matches!(
            *encoding,
            "NONE" | "AUTO" | "ANSI" | "OEM" | "UTF-8" | "UTF8"
        )
    {
        interp.issue(
            MessageType::AuthorWarning,
            &format!("ENCODING option given unknown value \"{encoding}\".  Ignoring."),
        );
    }
    let mut captures = ["OUTPUT", "ERROR"].map(|stream| Capture {
        quiet: options.flags.contains(format!("{stream}_QUIET").as_str()),
        capture: !options.get(&format!("{stream}_VARIABLE")).is_empty(),
        echo: options
            .flags
            .contains(format!("ECHO_{stream}_VARIABLE").as_str()),
        strip: options
            .flags
            .contains(format!("{stream}_STRIP_TRAILING_WHITESPACE").as_str()),
        ..Capture::default()
    });
    let statuses =
        run_pipeline(&options, &mut captures, timeout).map_err(|error| error.to_string())?;

    let out = options.get("OUTPUT_VARIABLE");
    let error = options.get("ERROR_VARIABLE");
    if !out.is_empty() {
        interp.set_def(out, captures[0].text());
    }
    if !error.is_empty() && error != out {
        interp.set_def(error, captures[1].text());
    }
    let result = options.get("RESULT_VARIABLE");
    let results = options.get("RESULTS_VARIABLE");
    if !result.is_empty() {
        interp.set_def(
            result,
            statuses
                .as_ref()
                .and_then(|all| all.last())
                .map_or_else(|| TIMEOUT.into(), Status::result),
        );
    }
    if !results.is_empty() {
        interp.set_def(
            results,
            statuses.as_ref().map_or_else(
                || TIMEOUT.into(),
                |all| all.iter().map(Status::result).collect::<Vec<_>>().join(";"),
            ),
        );
    }

    let failure = match (fatal.as_str(), statuses) {
        ("ANY" | "LAST", None) => Some(TIMEOUT.to_owned()),
        ("ANY", Some(statuses)) => {
            let errors: String = statuses
                .iter()
                .enumerate()
                .filter_map(|(index, status)| {
                    status
                        .failure()
                        .map(|error| format!("  {}: \"{error}\"\n", index + 1))
                })
                .collect();
            (!errors.is_empty()).then(|| format!("failed command indexes:\n{errors}"))
        }
        ("LAST", Some(statuses)) => match statuses.last() {
            Some(Status::Abnormal(message)) => Some(format!("Abnormal exit: {message}")),
            Some(Status::Exit(code)) if *code != 0 => Some("last command failed".into()),
            _ => None,
        },
        _ => None,
    };
    if let Some(error) = failure {
        interp.set_fatal();
        return err(error);
    }
    Ok(())
}
