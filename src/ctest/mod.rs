//! Standalone CTest driver for generated CTest test files.

mod buildandtest;
mod options;
mod process;
mod report;
mod resources;
mod runner;
pub(crate) mod script;
pub(crate) mod testfile;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::util::{collapse_full_path, expand_list};

use options::{Options, Show};

pub(crate) fn sibling(name: &str) -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.canonicalize().ok())
        .map_or_else(
            || PathBuf::from(".").join(name),
            |path| path.with_file_name(name),
        )
}

struct Log {
    quiet: bool,
    suppress_output: bool,
    extra: bool,
    progress: bool,
    progress_line: bool,
    file: Option<std::fs::File>,
    pub(in crate::ctest) system_error: bool,
}

impl Log {
    fn new(options: &Options) -> Self {
        Self {
            quiet: options.quiet || options.show == Show::Json,
            suppress_output: false,
            extra: options.verbose,
            progress: options.progress
                && options.output_log.is_none()
                && crate::stdio::stdout_is_vt100(),
            progress_line: false,
            system_error: false,
            file: options.output_log.as_ref().and_then(|path| {
                if options.script {
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(path)
                        .ok()
                } else {
                    std::fs::File::create(path).ok()
                }
            }),
        }
    }

    fn output(&mut self, text: &str) {
        self.write(text, false);
    }

    fn error(&mut self, text: &str) {
        self.system_error = true;
        self.write(text, true);
    }

    fn verbose(&mut self, text: &str) {
        if self.extra {
            self.output(text);
        }
    }

    fn direct(&self, text: &str) {
        let mut stream = io::stdout().lock();
        let _ = stream.write_all(&crate::bytes::encode(text));
        let _ = stream.flush();
    }

    fn write(&mut self, text: &str, error: bool) {
        if let Some(file) = &mut self.file {
            let _ = file.write_all(&crate::bytes::encode(text));
        }
        if !self.quiet {
            if error {
                let mut stream = io::stderr().lock();
                let _ = stream.write_all(&crate::bytes::encode(text));
                let _ = stream.flush();
            } else if !self.suppress_output {
                self.direct(text);
            }
        }
    }

    fn progress(&mut self, text: &str) {
        if !self.quiet {
            if self.progress_line {
                self.direct("\r");
            }
            self.progress_line = text.contains('\n');
            self.direct(&text.replace('\n', ""));
            self.direct("\x1b[K");
        }
    }
}

pub(in crate::ctest) fn directory_error(dir: &Path) -> String {
    let error = if dir.exists() {
        "Not a directory"
    } else {
        "No such file or directory"
    };
    format!(
        "Failed to change working directory to \"{}\": {error}",
        dir.display()
    )
}

fn xml_escape_launch(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn launcher_labels(current_build_dir: &str, target_name: &str, source: &str) -> Vec<String> {
    if current_build_dir.is_empty() || target_name.is_empty() {
        return Vec::new();
    }
    let file = Path::new(current_build_dir)
        .join("CMakeFiles")
        .join(format!("{target_name}.dir"))
        .join("Labels.txt");
    let Ok(contents) = std::fs::read_to_string(file) else {
        return Vec::new();
    };
    let source = source.replace('\\', "/");
    let mut labels = Vec::new();
    let mut in_target = true;
    let mut in_source = false;
    for line in contents.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(label) = line.strip_prefix(' ') {
            if in_target || in_source {
                let label = label.to_owned();
                if !labels.contains(&label) {
                    labels.push(label);
                }
            }
        } else if !source.is_empty() && !in_source {
            in_target = false;
            in_source = line.replace('\\', "/") == source;
        } else {
            break;
        }
    }
    labels.sort();
    labels
}

struct LaunchReport<'a> {
    log_dir: &'a Path,
    hash: &'a str,
    error: bool,
    options: &'a BTreeMap<String, String>,
    real_args: &'a [String],
    stdout: &'a str,
    stderr: &'a str,
    exit_code: i32,
}

fn launcher_xml(report: &LaunchReport) {
    let log_dir = report.log_dir;
    let hash = report.hash;
    let error = report.error;
    let options = report.options;
    let real_args = report.real_args;
    let stdout = report.stdout;
    let stderr = report.stderr;
    let exit_code = report.exit_code;
    let write_output = |tag: &str, text: &str, xml: &mut String| {
        xml.push_str(&format!("    <{tag}>"));
        let filter = options.get("filter-prefix").map_or("", String::as_str);
        let mut first = true;
        for line in text.lines() {
            if !filter.is_empty() && line.starts_with(filter) {
                continue;
            }
            if !first {
                xml.push('\n');
            }
            first = false;
            xml.push_str(&xml_escape_launch(line));
        }
        xml.push_str(&format!("</{tag}>\n"));
    };
    let mut xml = String::new();
    xml.push_str(&format!(
        "  <Failure type=\"{}\">\n",
        if error { "Error" } else { "Warning" }
    ));
    xml.push_str("    <Action>\n");
    for (key, tag) in [
        ("target-name", "TargetName"),
        ("language", "Language"),
        ("source", "SourceFile"),
        ("output", "OutputFile"),
    ] {
        if let Some(value) = options.get(key).filter(|value| !value.is_empty()) {
            let _ = writeln!(xml, "      <{tag}>{}</{tag}>", xml_escape_launch(value));
        }
    }
    let output_type = match options.get("target-type").map_or("", String::as_str) {
        "EXECUTABLE" => "executable",
        "SHARED_LIBRARY" => "shared library",
        "MODULE_LIBRARY" => "module library",
        "STATIC_LIBRARY" => "static library",
        _ if options
            .get("source")
            .is_some_and(|source| !source.is_empty()) =>
        {
            "object file"
        }
        _ => "",
    };
    if !output_type.is_empty() {
        let _ = writeln!(xml, "      <OutputType>{output_type}</OutputType>");
    }
    xml.push_str("    </Action>\n    <Command>\n");
    if let Ok(cwd) = std::env::current_dir() {
        let _ = writeln!(
            xml,
            "      <WorkingDirectory>{}</WorkingDirectory>",
            xml_escape_launch(&cwd.to_string_lossy())
        );
    }
    for arg in real_args {
        let _ = writeln!(xml, "      <Argument>{}</Argument>", xml_escape_launch(arg));
    }
    xml.push_str("    </Command>\n    <Result>\n");
    write_output("StdOut", stdout, &mut xml);
    write_output("StdErr", stderr, &mut xml);
    let _ = writeln!(xml, "      <ExitCondition>{exit_code}</ExitCondition>");
    xml.push_str("    </Result>\n");
    let labels = launcher_labels(
        options.get("current-build-dir").map_or("", String::as_str),
        options.get("target-name").map_or("", String::as_str),
        options.get("source").map_or("", String::as_str),
    );
    if !labels.is_empty() {
        xml.push_str("    <Labels>\n");
        for label in labels {
            let _ = writeln!(xml, "      <Label>{}</Label>", xml_escape_launch(&label));
        }
        xml.push_str("    </Labels>\n");
    }
    xml.push_str("  </Failure>\n");
    let kind = if error { "error" } else { "warning" };
    let _ = std::fs::write(log_dir.join(format!("{kind}-{hash}.xml")), xml);
}

fn instrument_launch(
    command: &[String],
    options: &BTreeMap<String, String>,
    status: i32,
    clock: crate::instrumentation::Timer,
    instrumentation: &crate::instrumentation::Instrumentation,
) {
    let build_dir = options.get("build-dir").map_or("", String::as_str);
    if build_dir.is_empty() {
        return;
    }
    let command_type = options.get("command-type").map_or("", String::as_str);
    let role = options
        .get("role")
        .filter(|role| !role.is_empty())
        .map_or(command_type, String::as_str);
    let comma_array = |key: &str| {
        serde_json::Value::Array(
            options
                .get(key)
                .map_or("", String::as_str)
                .split(',')
                .filter(|value| !value.is_empty())
                .map(|value| serde_json::Value::String(value.to_owned()))
                .collect(),
        )
    };
    let mut fields = serde_json::Map::new();
    for (option, key) in [
        ("target-name", "target"),
        ("source", "source"),
        ("language", "language"),
        ("target-type", "targetType"),
        ("config", "config"),
    ] {
        if let Some(value) = options.get(option).filter(|value| !value.is_empty()) {
            let value = if key == "language" && value == "CXX" {
                "C++"
            } else {
                value
            };
            fields.insert(key.into(), serde_json::Value::String(value.to_owned()));
        }
    }
    if options.get("output").is_some_and(|value| !value.is_empty()) {
        let outputs = comma_array("output");
        let sizes = outputs
            .as_array()
            .map(|outputs| {
                outputs
                    .iter()
                    .filter_map(|output| output.as_str())
                    .map(|output| {
                        let path = std::path::PathBuf::from(output);
                        let path = if path.is_absolute() {
                            path
                        } else {
                            std::path::PathBuf::from(build_dir).join(path)
                        };
                        serde_json::Value::from(
                            path.metadata().map(|metadata| metadata.len()).unwrap_or(0),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        fields.insert("outputSizes".into(), serde_json::Value::Array(sizes));
        fields.insert("outputs".into(), outputs);
    }
    if role == "link"
        && options
            .get("target-labels")
            .is_some_and(|value| !value.is_empty())
    {
        fields.insert("targetLabels".into(), comma_array("target-labels"));
    }
    instrumentation.write_snippet_fields(
        role,
        command,
        clock,
        status,
        serde_json::Value::Object(fields),
    );
}

fn launch(args: &[String], instrument: bool) -> i32 {
    let Some(separator) = args.iter().position(|arg| arg == "--") else {
        eprintln!("No launch/command separator ('--') found!");
        return 1;
    };
    let command = &args[separator + 1..];
    if command.is_empty() || command[0] == ":" {
        return 0;
    }
    let mut options = BTreeMap::new();
    let mut index = 0;
    while index < separator {
        let arg = &args[index];
        if let Some(name) = arg.strip_prefix("--")
            && index + 1 < separator
        {
            options.insert(name.to_owned(), args[index + 1].clone());
            index += 2;
        } else {
            index += 1;
        }
    }
    let real_args: Vec<String> = command.to_vec();
    let launch_instrumentation = if instrument {
        let build_dir = options.get("build-dir").map_or("", String::as_str);
        let mut instrumentation = crate::instrumentation::Instrumentation::new(build_dir);
        instrumentation.load();
        Some(instrumentation)
    } else {
        None
    };
    let clock = launch_instrumentation.as_ref().map_or_else(
        || crate::instrumentation::Timer::start(false),
        |instrumentation| instrumentation.timer(),
    );
    let log_dir = std::env::var("CTEST_LAUNCH_LOGS")
        .ok()
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from);
    let passthru = log_dir.is_none();
    if passthru {
        let status = std::process::Command::new(&command[0])
            .args(&command[1..])
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status();
        let code = match status {
            Ok(status) => status.code().unwrap_or(1),
            Err(_) => 1,
        };
        if instrument {
            if let Some(instrumentation) = &launch_instrumentation {
                instrument_launch(&real_args, &options, code, clock, instrumentation);
            }
        }
        return code;
    }
    let log_dir = log_dir.unwrap();
    let _ = std::fs::create_dir_all(&log_dir);
    let mut child = match std::process::Command::new(&command[0])
        .args(&command[1..])
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return 1,
    };
    fn forward<R: std::io::Read + Send + 'static, W: std::io::Write + Send + 'static>(
        mut input: R,
        mut output: W,
    ) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut captured = Vec::new();
            let mut buffer = [0; 8192];
            loop {
                match input.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        captured.extend_from_slice(&buffer[..count]);
                        let _ = output.write_all(&buffer[..count]);
                        let _ = output.flush();
                    }
                    Err(_) => break,
                }
            }
            captured
        })
    }
    let stdout_thread = child
        .stdout
        .take()
        .map(|pipe| forward(pipe, std::io::stdout()));
    let stderr_thread = child
        .stderr
        .take()
        .map(|pipe| forward(pipe, std::io::stderr()));
    let status = child
        .wait()
        .ok()
        .and_then(|status| status.code())
        .unwrap_or(1);
    let stdout = stdout_thread
        .and_then(|thread| thread.join().ok())
        .unwrap_or_default();
    let stderr = stderr_thread
        .and_then(|thread| thread.join().ok())
        .unwrap_or_default();
    let stdout_text = crate::bytes::decode(&stdout);
    let stderr_text = crate::bytes::decode(&stderr);
    use md5::Digest;
    let mut hasher = md5::Md5::new();
    if let Ok(cwd) = std::env::current_dir() {
        hasher.update(cwd.to_string_lossy().as_bytes());
    }
    for arg in &real_args {
        hasher.update(arg.as_bytes());
    }
    let hash = format!("{:x}", hasher.finalize());
    let base = format!("launch-{hash}");
    let _ = std::fs::write(log_dir.join(format!("{base}-out.txt")), &stdout);
    let _ = std::fs::write(log_dir.join(format!("{base}-err.txt")), &stderr);
    let warning = status == 0
        && [&stdout_text, &stderr_text].into_iter().any(|text| {
            text.lines().any(|line| {
                let lower = line.to_ascii_lowercase();
                lower.contains("warning") || lower.contains("remark") || lower.contains("note")
            })
        });
    if status != 0 || warning {
        launcher_xml(&LaunchReport {
            log_dir: &log_dir,
            hash: &hash,
            error: status != 0,
            options: &options,
            real_args: &real_args,
            stdout: &stdout_text,
            stderr: &stderr_text,
            exit_code: status,
        });
    }
    let _ = std::fs::remove_file(log_dir.join(format!("{base}-out.txt")));
    let _ = std::fs::remove_file(log_dir.join(format!("{base}-err.txt")));
    if instrument {
        if let Some(instrumentation) = &launch_instrumentation {
            instrument_launch(&real_args, &options, status, clock, instrumentation);
        }
    }
    status
}

pub fn main() -> i32 {
    let args: Vec<_> = std::env::args_os()
        .skip(1)
        .map(|arg| crate::bytes::decode(arg.as_encoded_bytes()))
        .collect();
    if args.first().is_some_and(|arg| arg == "--launch") {
        return launch(&args[1..], false);
    }
    if args.first().is_some_and(|arg| arg == "--instrument") {
        return launch(&args[1..], true);
    }
    if args.len() == 2 && args[0] == "--collect-instrumentation" {
        let mut instrumentation = crate::instrumentation::Instrumentation::new(&args[1]);
        instrumentation.load();
        instrumentation.collect("manual");
        return 0;
    }
    if args.len() == 2 && args[0] == "--start-instrumentation" {
        let mut instrumentation = crate::instrumentation::Instrumentation::new(&args[1]);
        instrumentation.load();
        instrumentation.collect("preBuild");
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("ctest"));
        let ppid = unsafe { libc::getppid() };
        let _ = std::process::Command::new(exe)
            .arg("--wait-and-collect-instrumentation")
            .arg(&args[1])
            .arg(ppid.to_string())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        return 0;
    }
    if args.len() == 3 && args[0] == "--wait-and-collect-instrumentation" {
        let ppid: i32 = args[2].parse().unwrap_or(0);
        while ppid > 0 && unsafe { libc::kill(ppid, 0) } == 0 {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let mut instrumentation = crate::instrumentation::Instrumentation::new(&args[1]);
        instrumentation.load();
        let clock = instrumentation.timer();
        instrumentation.write_snippet_fields("build", &[], clock, 0, serde_json::json!({}));
        instrumentation.collect("postBuild");
        return 0;
    }
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--version" | "-version" | "/V"))
    {
        println!(
            "ctest version {}\n\nCMake suite maintained and supported by Kitware (kitware.com/cmake).",
            crate::interp::VERSION_STRING
        );
        return 0;
    }
    let options = match Options::parse(&args) {
        Ok(options) => options,
        Err(message) => {
            if let Some(message) = message.strip_prefix(options::RAW_ERROR) {
                eprintln!("{message}");
            } else {
                eprintln!("CMake Error: {message}");
            }
            return 1;
        }
    };
    if options.list_presets {
        return 0;
    }
    if options.build_and_test.active {
        return buildandtest::run(&options.build_and_test, &options.config);
    }
    if !options.scripts.is_empty() {
        return script::run_scripts(&options);
    }
    if !options.dashboard_parts.is_empty() || !options.model.is_empty() {
        return script::process_steps(&options);
    }
    let mut log = Log::new(&options);
    if !matches!(options.no_tests.as_str(), "" | "error" | "ignore") {
        log.error(&format!(
            "Unknown value for CTEST_NO_TESTS_ACTION: '{}'",
            options.no_tests
        ));
        return 1;
    }
    let Ok(cwd) = std::env::current_dir() else {
        log.error("Current working directory cannot be established.\n");
        return 1;
    };
    let dir = options
        .dir
        .as_ref()
        .filter(|dir| !dir.as_os_str().is_empty())
        .map_or(cwd.clone(), |dir| {
            PathBuf::from(collapse_full_path(
                &dir.to_string_lossy(),
                &cwd.to_string_lossy(),
            ))
        });
    if let Err(error) = std::env::set_current_dir(&dir) {
        let error = error
            .to_string()
            .split(" (os error")
            .next()
            .unwrap_or_default()
            .to_owned();
        log.error(&format!(
            "Failed to change working directory to \"{}\": {error}\n",
            dir.display()
        ));
        return 1;
    }
    execute(dir, options, &mut log)
}

fn read_configuration(dir: &Path, log: &mut Log, verbose_config: bool) -> BTreeMap<String, String> {
    let mut file = dir.join("CTestConfiguration.ini");
    if !file.exists() {
        file = dir.join("DartConfiguration.tcl");
    }
    if verbose_config {
        log.verbose(&format!(
            "UpdateCTestConfiguration  from :{}\n",
            file.display()
        ));
    }
    let Ok(contents) = std::fs::read_to_string(&file) else {
        return BTreeMap::new();
    };
    log.verbose(&format!("Parse Config file:{}\n", file.display()));
    let mut config = BTreeMap::new();
    let mut lines = contents.lines();
    while let Some(line) = lines.next() {
        let mut line = line.trim().to_owned();
        while line.ends_with('\\') {
            line.pop();
            let Some(next) = lines.next() else { break };
            line.push_str(next.trim());
        }
        if !line.starts_with('#')
            && let Some((key, value)) = line.split_once(':')
        {
            config.insert(key.into(), value.trim().into());
        }
    }
    config
}

fn read_custom(dir: &Path, log: &mut Log, memcheck: bool) -> Vec<String> {
    let file = ["CTestCustom.cmake", "CTestCustom.ctest"]
        .into_iter()
        .map(|filename| dir.join(filename))
        .find(|path| path.exists());
    let Some(file) = file else { return Vec::new() };
    let mut interp = testfile::interpreter(dir);
    if !interp.read_list_file(&file.to_string_lossy()) || interp.error || interp.fatal {
        log.error(&format!(
            "Problem reading custom configuration: {}\n",
            file.display()
        ));
    }
    expand_list(
        &interp.get_safe(if memcheck {
            "CTEST_CUSTOM_MEMCHECK_IGNORE"
        } else {
            "CTEST_CUSTOM_TESTS_IGNORE"
        }),
        false,
    )
}

fn execute(dir: PathBuf, options: Options, log: &mut Log) -> i32 {
    execute_detailed(dir, options, log).0
}

pub(in crate::ctest) fn execute_detailed(
    mut dir: PathBuf,
    mut options: Options,
    log: &mut Log,
) -> (i32, bool) {
    let config = read_configuration(&dir, log, !options.script);
    if options.labels_for_subprojects.is_empty()
        && let Some(value) = config.get("LabelsForSubprojects")
    {
        options.labels_for_subprojects = crate::util::expand_list(value, false);
    }
    let binary = if options.preset_binary_dir.is_empty() {
        config
            .get("BuildDirectory")
            .filter(|value| !value.is_empty())
            .map_or_else(|| dir.clone(), PathBuf::from)
    } else {
        PathBuf::from(&options.preset_binary_dir)
    };
    if options.dir.is_none() && binary.is_dir() && std::env::set_current_dir(&binary).is_ok() {
        dir = binary.clone();
    }
    let mut ignore = read_custom(&binary, log, !options.memcheck_type.is_empty());
    ignore.extend(options.memcheck_ignore.iter().cloned());
    if !options.script_quiet && options.memcheck_type.is_empty() {
        log.output(&format!("Test project {}\n", dir.display()));
    }
    if options.script
        && !options.output_truncation.is_empty()
        && !matches!(
            options.output_truncation.as_str(),
            "head" | "middle" | "tail"
        )
    {
        log.error(&format!(
            "Invalid value for CTEST_CUSTOM_TEST_OUTPUT_TRUNCATION: {}\n",
            options.output_truncation
        ));
        log.system_error = true;
        return (8, false);
    }
    let started = Instant::now();
    let start_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64);
    let date = report::clock("%Y-%m-%dT%H:%M:%S");
    let temporary = binary.join("Testing/Temporary");
    let _ = std::fs::create_dir_all(&temporary);
    let last_name = if options.dashboard_tag.is_empty() {
        "LastTest.log".into()
    } else {
        format!("LastTest_{}.log", options.dashboard_tag)
    };
    let last_path = temporary.join(last_name);
    let mut no_tests = false;
    let result = testfile::discover(&dir, &mut options, ignore, log).and_then(|all| {
        let mut tests = testfile::select(&all, &options, &temporary, log)?;
        let mut last_log = report::start_log();
        let mut instrumentation = crate::instrumentation::Instrumentation::new(&binary);
        instrumentation.load();
        let instrument_tests = instrumentation.enabled && !options.script;
        let ctest_clock = instrumentation.timer();
        let context = runner::RunContext {
            options: &options,
            all_count: all.len(),
            temporary: &temporary,
            config: &config,
        };
        let results = match (options.show, options.print_labels) {
            (_, true) => {
                if options.show == Show::Run && runner::validate(&tests, &options, log).is_err() {
                    let _ = std::fs::write(&last_path, crate::bytes::encode(&last_log));
                    return Err(());
                }
                let labels: BTreeSet<_> =
                    tests.iter().flat_map(|test| test.list("LABELS")).collect();
                if labels.is_empty() {
                    log.output("No Labels Exist\n");
                } else {
                    log.output("All Labels:\n");
                    for label in labels {
                        log.output(&format!("  {label}\n"));
                    }
                }
                None
            }
            (Show::Human, false) => {
                report::show_human(&tests, &options, log);
                None
            }
            (Show::Json, false) => {
                report::show_json(&tests, &options, log);
                None
            }
            (Show::Run, false) => match runner::run(&mut tests, &context, log, &mut last_log) {
                Ok(results) => Some(results),
                Err(()) => {
                    let _ = std::fs::write(&last_path, crate::bytes::encode(&last_log));
                    return Err(());
                }
            },
        };
        if instrument_tests {
            let invocation: Vec<String> = std::env::args_os()
                .skip(1)
                .map(|arg| crate::bytes::decode(arg.as_encoded_bytes()))
                .collect();
            let ctest_result = if results.as_ref().is_some_and(|results| {
                results
                    .iter()
                    .flatten()
                    .any(|result| result.status.failed())
            }) {
                1
            } else {
                0
            };
            instrumentation.write_snippet_fields(
                "ctest",
                &invocation,
                ctest_clock,
                ctest_result,
                serde_json::json!({}),
            );
            if let Some(results) = &results {
                for (test, result) in tests
                    .iter()
                    .zip(results.iter())
                    .filter_map(|(test, result)| result.as_ref().map(|result| (test, result)))
                {
                    if result.status.not_run() || result.actual.is_empty() {
                        continue;
                    }
                    let mut command = Vec::with_capacity(result.args.len() + 1);
                    command.push(result.actual.clone());
                    command.extend(result.args.iter().cloned());
                    instrumentation.write_snippet_fields(
                        "test",
                        &command,
                        instrumentation.timer(),
                        result.exit_value,
                        serde_json::json!({
                            "testName": test.name,
                            "config": options.config,
                        }),
                    );
                }
            }
        }
        if !options.dashboard_tag.is_empty()
            && let Some(results) = &results
        {
            let _ = report::test_xml(&binary, &options, &tests, results, start_epoch);
        }
        let _ = writeln!(last_log, "End testing: {}", report::clock("%b %d %H:%M %Z"));
        if let Some(results) = &results {
            no_tests = results
                .iter()
                .flatten()
                .filter(|result| result.status != process::Status::Disabled)
                .count()
                == 0
                && options.no_tests != "ignore";
        }
        let success = results.as_ref().is_none_or(|results| {
            runner::summarize(
                &tests,
                results,
                &options,
                &temporary,
                started,
                log,
                &mut last_log,
            )
        });
        if instrument_tests {
            instrumentation.collect("postTest");
        }
        let _ = std::fs::write(&last_path, crate::bytes::encode(&last_log));
        if let Some(path) = &options.junit {
            let build = config.get("BuildName").map_or("", String::as_str);
            let site = config.get("Site").map_or("", String::as_str);
            if report::junit(
                path,
                &tests,
                results.as_deref().unwrap_or(&[]),
                build,
                site,
                &date,
                started.elapsed().as_secs_f64(),
            )
            .is_err()
            {
                log.error(&format!("Problem opening file: {}\n", path.display()));
                return Ok(true);
            }
        }
        Ok(success)
    });
    if result == Ok(true) {
        (0, no_tests)
    } else {
        if !options.script {
            log.error("Errors while running CTest\n");
            if !options.output_on_failure {
                log.error(&format!("Output from these tests are in: {}\nUse \"--rerun-failed --output-on-failure\" to re-run the failed cases verbosely.\n", last_path.display()));
            }
        }
        (8, no_tests)
    }
}
