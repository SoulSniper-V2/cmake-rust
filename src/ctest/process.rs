use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use crate::util::{collapse_full_path, expand_list};

use super::Log;
use super::options::{Options, integer, number};
use super::testfile::{Test, matches, regex};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Status {
    Passed,
    Failed,
    Timeout,
    Signal(&'static str),
    BadCommand,
    NotRun,
    Skipped,
    Disabled,
}

impl Status {
    pub fn description(self) -> &'static str {
        match self {
            Self::Passed => "Completed",
            Self::Failed => "Failed",
            Self::Timeout => "Timeout",
            Self::Signal(name) => name,
            Self::BadCommand => "BAD_COMMAND",
            Self::NotRun | Self::Skipped | Self::Disabled => "Not Run",
        }
    }

    pub fn failed(self) -> bool {
        !matches!(self, Self::Passed | Self::Skipped | Self::Disabled)
    }

    pub fn not_run(self) -> bool {
        matches!(self, Self::NotRun | Self::Skipped | Self::Disabled)
    }
}

pub(super) struct ResultData {
    pub status: Status,
    pub output: String,
    pub reason: String,
    pub completion: String,
    pub exit_value: i32,
    pub seconds: f64,
    pub actual: String,
    pub args: Vec<String>,
    pub start: String,
    pub diagnostic: String,
    pub timeout_signal: Option<String>,
    pub stop_time_passed: bool,
    pub alternate_used: bool,
    pub completion_order: usize,
    pub labels: Vec<String>,
    pub environment: String,
    pub measurements: Vec<String>,
}

impl ResultData {
    pub fn not_started(status: Status, completion: &str, diagnostic: String) -> Self {
        Self {
            status,
            output: if status == Status::Disabled {
                "Disabled".into()
            } else {
                diagnostic.clone()
            },
            reason: String::new(),
            completion: completion.into(),
            exit_value: -1,
            seconds: 0.0,
            actual: String::new(),
            args: Vec::new(),
            start: String::new(),
            diagnostic,
            timeout_signal: None,
            stop_time_passed: false,
            alternate_used: false,
            completion_order: 0,
            labels: Vec::new(),
            environment: String::new(),
            measurements: Vec::new(),
        }
    }
}

pub(super) enum Event {
    Output(usize, String),
    Error(String),
    Done(usize, Box<ResultData>),
}

pub(super) fn executable(name: &str, dir: &Path, config: &str, log: &mut Log) -> String {
    if name == "NOT_AVAILABLE" {
        return name.into();
    }
    let (path, basename) = name
        .rsplit_once('/')
        .map_or(("", name), |(p, n)| (&name[..p.len() + 1], n));
    let mut attempts = Vec::new();
    for path in std::iter::once(path).chain(path.strip_prefix('/')) {
        attempts.push(format!("{path}{basename}"));
        if config.is_empty() {
            for config in [
                "Release",
                "Debug",
                "MinSizeRel",
                "RelWithDebInfo",
                "Deployment",
                "Development",
            ] {
                attempts.push(format!("{path}{config}/{basename}"));
            }
        } else {
            attempts.push(format!("{path}{config}/{basename}"));
            attempts.push(format!("{config}/{path}{basename}"));
        }
    }
    for attempt in &attempts {
        if dir.join(attempt).is_file() {
            return collapse_full_path(attempt, &dir.to_string_lossy());
        }
    }
    if path.is_empty()
        && let Some(paths) = std::env::var_os("PATH")
    {
        for entry in std::env::split_paths(&paths) {
            let candidate = dir.join(entry).join(basename);
            if candidate
                .metadata()
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            {
                return collapse_full_path(&candidate.to_string_lossy(), &dir.to_string_lossy());
            }
        }
    }
    log.output(&format!(
        "Could not find executable {name}\nLooked in the following places:\n"
    ));
    for attempt in attempts {
        log.output(&format!("{attempt}\n{attempt}\n"));
    }
    String::new()
}

pub(super) fn command_info(test: &Test, config: &str, log: &mut Log) -> String {
    let actual = executable(&test.command[0], &test.dir, config, log);
    let command_line =
        test.command[1..]
            .iter()
            .fold(actual.replace(' ', "\\ "), |mut line, arg| {
                use std::fmt::Write;
                let _ = write!(line, " \"{arg}\"");
                line
            });
    let index = test.index;
    log.verbose(&format!("\n{index}: Test command: {command_line}\n"));
    if !test.dir.as_os_str().is_empty() {
        log.verbose(&format!(
            "{index}: Working Directory: {}\n",
            test.dir.display()
        ));
    }
    for (property, description) in [
        ("ENVIRONMENT", "Environment variables"),
        (
            "ENVIRONMENT_MODIFICATION",
            "Environment variable modifications",
        ),
    ] {
        let values = test.list(property);
        if !values.is_empty() {
            log.verbose(&format!("{index}: {description}: \n"));
            for value in values {
                log.verbose(&format!("{index}:  {value}\n"));
            }
        }
    }
    actual
}

type Environment = BTreeMap<String, Option<OsString>>;

fn environment(test: &Test, options: &Options) -> Result<Environment, Vec<String>> {
    let mut base = Environment::new();
    if options.interactive_debug {
        base.insert("CTEST_INTERACTIVE_DEBUG_MODE".into(), Some("1".into()));
    } else {
        base.insert("DART_TEST_FROM_DART".into(), Some("1".into()));
        base.insert(
            "DASHBOARD_TEST_FROM_CTEST".into(),
            Some(crate::interp::VERSION_STRING.into()),
        );
        base.insert("CTEST_INTERACTIVE_DEBUG_MODE".into(), None);
    }
    for assignment in test.list("ENVIRONMENT") {
        let (name, value) = assignment.split_once('=').unwrap_or((assignment, ""));
        base.insert(name.into(), Some(value.into()));
    }
    let mut modified = Environment::new();
    let mut errors = Vec::new();
    for spec in test.list("ENVIRONMENT_MODIFICATION") {
        let Some((name, operation)) = spec.split_once('=') else {
            errors.push(format!(
                "Error: Missing `=` after the variable name in: {spec}\n\n"
            ));
            continue;
        };
        let Some((op, value)) = operation.split_once(':') else {
            errors.push(format!(
                "Error: Missing `:` after the operation in: {spec}\n\n"
            ));
            continue;
        };
        let value = match op {
            "reset" => {
                modified.remove(name);
                continue;
            }
            "set" => Some(value.into()),
            "unset" => None,
            "string_append" | "string_prepend" | "path_list_append" | "path_list_prepend"
            | "cmake_list_append" | "cmake_list_prepend" => {
                let lookup = || {
                    base.get(name)
                        .cloned()
                        .unwrap_or_else(|| std::env::var_os(name))
                };
                let current = modified
                    .get(name)
                    .cloned()
                    .flatten()
                    .or_else(lookup)
                    .unwrap_or_default();
                let separator = if current.is_empty() || op.starts_with("string") {
                    ""
                } else if op.starts_with("path") {
                    ":"
                } else {
                    ";"
                };
                let mut result = OsString::new();
                if op.ends_with("append") {
                    result.push(current);
                    result.push(separator);
                    result.push(value);
                } else {
                    result.push(value);
                    result.push(separator);
                    result.push(current);
                }
                Some(result)
            }
            _ => {
                errors.push(format!(
                    "Error: Unrecognized environment manipulation argument: {op}\n\n"
                ));
                continue;
            }
        };
        modified.insert(name.into(), value);
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    base.extend(modified);
    base.insert("CTEST_RESOURCE_GROUP_COUNT".into(), None);
    Ok(base)
}

pub(super) fn timeout_signal(name: &str) -> Option<i32> {
    match name {
        "SIGINT" => Some(libc::SIGINT),
        "SIGQUIT" => Some(libc::SIGQUIT),
        "SIGTERM" => Some(libc::SIGTERM),
        "SIGUSR1" => Some(libc::SIGUSR1),
        "SIGUSR2" => Some(libc::SIGUSR2),
        _ => None,
    }
}

pub(super) fn property_errors(test: &Test) -> String {
    test.property_errors.join("\n")
}

pub(super) fn processors_available() -> BTreeSet<usize> {
    // SAFETY: the mask is initialized before use and sched_getaffinity writes
    // at most size_of::<cpu_set_t>() bytes into it.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        if libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) != 0 {
            return BTreeSet::new();
        }
        (0..libc::CPU_SETSIZE as usize)
            .filter(|cpu| libc::CPU_ISSET(*cpu, &set))
            .collect()
    }
}

fn environment_measurement(test: &Test, resource_env: &BTreeMap<String, String>) -> String {
    let mut values: BTreeMap<String, Option<String>> = BTreeMap::new();
    for assignment in test.list("ENVIRONMENT") {
        let (name, value) = assignment.split_once('=').unwrap_or((assignment, ""));
        values.insert(name.into(), Some(value.into()));
    }
    for spec in test.list("ENVIRONMENT_MODIFICATION") {
        let Some((name, operation)) = spec.split_once('=') else {
            continue;
        };
        let Some((op, value)) = operation.split_once(':') else {
            continue;
        };
        match op {
            "reset" => {
                values.remove(name);
            }
            "set" => {
                values.insert(name.into(), Some(value.into()));
            }
            "unset" => {
                values.insert(name.into(), None);
            }
            "string_append" | "string_prepend" | "path_list_append" | "path_list_prepend"
            | "cmake_list_append" | "cmake_list_prepend" => {
                let separator = if op.starts_with("path") {
                    ":"
                } else if op.starts_with("cmake") {
                    ";"
                } else {
                    ""
                };
                let current = values
                    .get(name)
                    .cloned()
                    .flatten()
                    .or_else(|| std::env::var(name).ok())
                    .unwrap_or_default();
                let separator = if current.is_empty() { "" } else { separator };
                let updated = if op.ends_with("prepend") {
                    format!("{value}{separator}{current}")
                } else {
                    format!("{current}{separator}{value}")
                };
                values.insert(name.into(), Some(updated));
            }
            _ => {}
        }
    }
    for (name, value) in resource_env {
        values.insert(name.clone(), Some(value.clone()));
    }
    let resource_index = |name: &str| -> Option<(usize, usize, usize)> {
        let rest = name.strip_prefix("CTEST_RESOURCE_GROUP_")?;
        if rest == "COUNT" {
            return Some((0, 0, 0));
        }
        let mut parts = rest.split('_');
        let group = parts.next()?.parse::<usize>().ok()?;
        let kind_order = usize::from(parts.next().is_some());
        Some((group + 1, kind_order, name.len()))
    };
    let mut ordered: Vec<_> = values.iter().collect();
    ordered.sort_by_key(|(name, _)| {
        resource_index(name).map_or((usize::MAX, usize::MAX, usize::MAX), |key| key)
    });
    let resource_count = ordered
        .iter()
        .filter(|(name, _)| resource_index(name).is_some())
        .count();
    let mut lines = Vec::new();
    for (name, value) in ordered
        .iter()
        .filter(|(name, _)| resource_index(name).is_none())
    {
        lines.push(
            value
                .as_ref()
                .map_or_else(|| format!("#{name}="), |value| format!("{name}={value}")),
        );
    }
    for (name, value) in ordered
        .iter()
        .filter(|(name, _)| resource_index(name).is_some())
    {
        lines.push(
            value
                .as_ref()
                .map_or_else(|| format!("#{name}="), |value| format!("{name}={value}")),
        );
    }
    let _ = resource_count;
    if resource_env.is_empty() {
        lines.push("#CTEST_RESOURCE_GROUP_COUNT=".into());
    }
    lines.join("\n")
}

pub(super) struct Invocation {
    pub exe: String,
    args: Vec<String>,
    dir: PathBuf,
    affinity: Vec<usize>,
    env: Environment,
    environment_measurement: String,
    timeout: f64,
    stop_at_timeout: bool,
    alternate: Option<(f64, Vec<String>)>,
    signal: Option<(String, i32, f64)>,
}

impl Invocation {
    pub fn prepare(
        test: &Test,
        exe: String,
        options: &Options,
        test_timeout: f64,
        resource_env: &BTreeMap<String, String>,
        affinity: Vec<usize>,
        log: &mut Log,
    ) -> Option<Self> {
        let fallback = if options.timeout > 0.0 {
            options.timeout
        } else {
            test_timeout
        };
        let timeout = test
            .properties
            .get("TIMEOUT")
            .map(|value| number(value))
            .filter(|timeout| *timeout >= 0.0)
            .unwrap_or(fallback);
        // MaxDuration is 10^7 seconds. CTest subtracts the historical
        // two-minute dashboard buffer from the remaining time.
        let stop_at_timeout = timeout <= 0.0;
        let timeout = if stop_at_timeout {
            9_999_879.0
        } else {
            timeout
        };
        log.verbose(&format!(
            "{}: Test timeout computed to be: {}\n",
            test.index, timeout as u64
        ));
        let mut env = match environment(test, options) {
            Ok(env) => env,
            Err(errors) => {
                for error in errors {
                    log.error(&error);
                }
                return None;
            }
        };
        env.extend(
            resource_env
                .iter()
                .map(|(name, value)| (name.clone(), Some(value.into()))),
        );
        let alternate = match expand_list(test.property("TIMEOUT_AFTER_MATCH"), false).as_slice() {
            [timeout, patterns] => Some((number(timeout), expand_list(patterns, false))),
            _ => None,
        };
        let signal_name = test.property("TIMEOUT_SIGNAL_NAME");
        let signal = timeout_signal(signal_name).map(|signal| {
            let grace = test
                .properties
                .get("TIMEOUT_SIGNAL_GRACE_PERIOD")
                .map_or(1.0, |value| number(value));
            (signal_name.to_owned(), signal, grace)
        });
        let mut exe = exe;
        let mut args = test.command[1..].to_vec();
        if !options.memcheck_type.is_empty() {
            let variable = match options.memcheck_type.as_str() {
                "AddressSanitizer" => "ASAN_OPTIONS",
                "LeakSanitizer" => "LSAN_OPTIONS",
                "ThreadSanitizer" => "TSAN_OPTIONS",
                "MemorySanitizer" => "MSAN_OPTIONS",
                _ => "UBSAN_OPTIONS",
            };
            let log_file = format!(
                "{}/MemoryChecker.{}.log",
                options.memcheck_log_dir, test.index
            );
            let extra = if options.memcheck_sanitizer_options.is_empty() {
                String::new()
            } else {
                format!(":{}", options.memcheck_sanitizer_options)
            };
            args.insert(0, exe.clone());
            args.insert(0, format!("{variable}=log_path='{log_file}'{extra}"));
            args.insert(0, "env".into());
            args.insert(0, "-E".into());
            exe = options.memcheck_cmake_command.clone();
        }
        Some(Self {
            exe,
            args,
            dir: test.dir.clone(),
            affinity,
            env,
            environment_measurement: environment_measurement(test, resource_env),
            timeout,
            stop_at_timeout,
            alternate,
            signal,
        })
    }

    pub fn spawn(self, test: Test, send: Sender<Event>) {
        std::thread::spawn(move || {
            let result = self.run(&test, &send);
            let _ = send.send(Event::Done(test.index, Box::new(result)));
        });
    }

    fn start_child(&self) -> io::Result<(std::process::Child, std::io::PipeReader)> {
        let (reader, writer) = std::io::pipe()?;
        let fd = reader.as_raw_fd();
        // SAFETY: reader is valid for both fcntl calls; only its read end is nonblocking.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags == -1 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) == -1 {
                return Err(io::Error::last_os_error());
            }
        }
        let mut command = Command::new(&self.exe);
        if !self.affinity.is_empty() {
            let affinity = self.affinity.clone();
            // SAFETY: this runs in the single-threaded child after fork and
            // only initializes a CPU mask before calling sched_setaffinity.
            unsafe {
                command.pre_exec(move || {
                    let mut set: libc::cpu_set_t = std::mem::zeroed();
                    for cpu in &affinity {
                        libc::CPU_SET(*cpu, &mut set);
                    }
                    if libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) == 0
                    {
                        Ok(())
                    } else {
                        Err(io::Error::last_os_error())
                    }
                });
            }
        }
        command
            .args(&self.args)
            .current_dir(&self.dir)
            .stdin(Stdio::inherit())
            .stdout(writer.try_clone()?)
            .stderr(writer)
            .process_group(0);
        for (name, value) in &self.env {
            if let Some(value) = value {
                command.env(name, value);
            } else {
                command.env_remove(name);
            }
        }
        command.spawn().map(|child| (child, reader))
    }

    fn run(self, test: &Test, send: &Sender<Event>) -> ResultData {
        let start = super::report::clock("%b %d %H:%M %Z");
        let started = Instant::now();
        let (mut child, mut reader) = match self.start_child() {
            Ok(process) => process,
            Err(error) => {
                let error = error
                    .to_string()
                    .split(" (os error")
                    .next()
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                let _ = send.send(Event::Error(format!(
                    "Process not started\n {}\n[{error}]\n",
                    self.exe
                )));
                let mut result =
                    ResultData::not_started(Status::BadCommand, "Failed to start", String::new());
                result.actual = self.exe;
                result.args = self.args;
                result.start = start;
                return result;
            }
        };

        let mut output = ProcessOutput::new(test, send, self.alternate);
        let mut status = None;
        let mut timeout_start = started;
        let mut timeout = if self.timeout > 0.0 {
            self.timeout
        } else {
            f64::INFINITY
        };
        let mut expired = false;
        let mut timeout_signal = None;
        let mut alternate_used = false;
        let mut exited = None;
        let mut eof = false;
        let mut buffer = [0; 8192];
        while !eof || status.is_none() {
            if !eof {
                match reader.read(&mut buffer) {
                    Ok(0) => eof = true,
                    Ok(len) => {
                        if let Some(alternate) = output.read(&buffer[..len]) {
                            timeout = alternate;
                            timeout_start = Instant::now();
                            alternate_used = true;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        if exited.is_some_and(|exited: Instant| exited.elapsed().as_secs() >= 2) {
                            eof = true;
                        } else {
                            std::thread::sleep(Duration::from_millis(2));
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => eof = true,
                }
            } else {
                std::thread::sleep(Duration::from_millis(2));
            }
            if status.is_none() {
                status = child.try_wait().ok().flatten();
                if status.is_some() {
                    exited = Some(Instant::now());
                }
            }
            if status.is_none() && timeout_start.elapsed().as_secs_f64() >= timeout {
                let pid = child.id() as i32;
                if !expired
                    && !self.stop_at_timeout
                    && let Some((name, number, grace)) = &self.signal
                {
                    // SAFETY: pid identifies the current child.
                    unsafe {
                        libc::kill(pid, *number);
                    }
                    timeout_signal = Some(name.clone());
                    timeout_start = Instant::now();
                    timeout = *grace;
                } else {
                    // SAFETY: the outstanding child leads its own process group.
                    unsafe {
                        libc::kill(-pid, libc::SIGKILL);
                    }
                    timeout_signal = None;
                    timeout = f64::INFINITY;
                }
                expired = true;
            }
        }
        if !output.partial.is_empty() {
            let partial = std::mem::take(&mut output.partial);
            output.line(&partial);
        }
        let status = status.unwrap();
        let exit_value = status.code().unwrap_or(-1);
        let (status, reason, mut completion) = classify(test, &output.text, expired, status);
        if let Some(details) = &output.details {
            completion = details.clone();
        }
        ResultData {
            status,
            output: output.text,
            reason,
            completion,
            exit_value,
            seconds: exited
                .unwrap_or_else(Instant::now)
                .duration_since(started)
                .as_secs_f64(),
            actual: self.exe,
            args: self.args,
            start,
            diagnostic: String::new(),
            timeout_signal,
            stop_time_passed: expired && self.stop_at_timeout,
            alternate_used,
            completion_order: 0,
            labels: output.labels,
            environment: self.environment_measurement,
            measurements: output.measurements,
        }
    }
}

struct ProcessOutput<'a> {
    test: &'a Test,
    send: &'a Sender<Event>,
    alternate: Option<(f64, Vec<String>)>,
    partial: Vec<u8>,
    text: String,
    labels: Vec<String>,
    details: Option<String>,
    measurements: Vec<String>,
}

impl<'a> ProcessOutput<'a> {
    fn new(test: &'a Test, send: &'a Sender<Event>, alternate: Option<(f64, Vec<String>)>) -> Self {
        Self {
            test,
            send,
            alternate,
            partial: Vec::new(),
            text: String::new(),
            labels: Vec::new(),
            details: None,
            measurements: Vec::new(),
        }
    }

    fn read(&mut self, bytes: &[u8]) -> Option<f64> {
        let mut next = bytes;
        let mut updated = None;
        while let Some(end) = next.iter().position(|byte| matches!(byte, b'\n' | 0)) {
            self.partial.extend_from_slice(&next[..end]);
            let mut line = std::mem::take(&mut self.partial);
            while line.last() == Some(&b'\r') {
                line.pop();
            }
            updated = self.line(&line).or(updated);
            next = &next[end + 1..];
        }
        self.partial.extend_from_slice(next);
        updated
    }

    fn line(&mut self, line: &[u8]) -> Option<f64> {
        let line = crate::bytes::decode(line);
        let index = self.test.index;
        let _ = self.send.send(Event::Output(index, line.clone()));
        if line.starts_with("<DartMeasurement") || line.starts_with("<CTestMeasurement") {
            self.measurements.push(line.clone());
            return None;
        }
        for (tag, notice) in [
            ("CTestDetails", "Test Details changed to"),
            ("CTestLabel", "Test Label added:"),
        ] {
            let begin = format!("<{tag}>");
            let end = format!("</{tag}>");
            if let Some((label, _)) = line
                .split_once(&begin)
                .and_then(|(_, contents)| contents.split_once(&end))
            {
                if tag != "CTestLabel"
                    || (!self.test.list("LABELS").iter().any(|item| item == label)
                        && !self.labels.iter().any(|item| item == label))
                {
                    let _ = self
                        .send
                        .send(Event::Output(index, format!("{notice} '{label}'")));
                    if tag == "CTestLabel" {
                        self.labels.push(label.into());
                    } else {
                        self.details = Some(label.into());
                    }
                }
                return None;
            }
        }
        self.text.push_str(&line);
        self.text.push('\n');
        if let Some((timeout, patterns)) = &self.alternate
            && patterns
                .iter()
                .any(|pattern| matches(&regex(pattern), &self.text))
        {
            let timeout = *timeout;
            let _ = self.send.send(Event::Output(
                index,
                format!("Test timeout changed to {}", timeout as u64),
            ));
            self.alternate = None;
            return Some(timeout);
        }
        None
    }
}

fn classify(
    test: &Test,
    output: &str,
    expired: bool,
    status: std::process::ExitStatus,
) -> (Status, String, String) {
    let mut reason = String::new();
    let mut force_fail = false;
    let required = test.list("PASS_REGULAR_EXPRESSION");
    let find_match = |patterns: &[String]| {
        patterns
            .iter()
            .position(|pattern| matches(&regex(pattern), output))
    };

    if !required.is_empty() {
        if let Some(found) = find_match(required) {
            reason = format!(
                "Required regular expression found. Regex=[{}]",
                required[found]
            );
        } else {
            reason = format!(
                "Required regular expression not found. Regex=[{}\n]",
                required.join("\n")
            );
            force_fail = true;
        }
    }
    let fails = test.list("FAIL_REGULAR_EXPRESSION");
    if let Some(found) = find_match(fails) {
        reason = format!(
            "Error regular expression found in output. Regex=[{}]",
            fails[found]
        );
        force_fail = true;
    }
    let skips = test.list("SKIP_REGULAR_EXPRESSION");
    let skipped = find_match(skips);
    if let Some(found) = skipped {
        reason = format!(
            "Skip regular expression found in output. Regex=[{}]",
            skips[found]
        );
    }
    let mut completion = String::from("Completed");
    let result = if expired {
        Status::Timeout
    } else if let Some(signal) = status.signal() {
        Status::Signal(match signal {
            libc::SIGSEGV | libc::SIGBUS => "SEGFAULT",
            libc::SIGILL => "ILLEGAL",
            libc::SIGINT => "INTERRUPT",
            libc::SIGFPE => "NUMERICAL",
            libc::SIGABRT => "Subprocess aborted",
            libc::SIGKILL => "Subprocess killed",
            _ => "OTHER_FAULT",
        })
    } else {
        let code = status.code().unwrap_or(-1);
        let skip_code = test
            .properties
            .get("SKIP_RETURN_CODE")
            .map(|value| integer(value));
        if skipped.is_some() {
            completion = "SKIP_REGULAR_EXPRESSION_MATCHED".into();
            Status::Skipped
        } else if (0..=255).contains(&code) && Some(code) == skip_code {
            completion = format!("SKIP_RETURN_CODE={code}");
            Status::Skipped
        } else if (!force_fail && (code == 0 || !required.is_empty())) != test.enabled("WILL_FAIL")
        {
            Status::Passed
        } else {
            Status::Failed
        }
    };
    (result, reason, completion)
}
