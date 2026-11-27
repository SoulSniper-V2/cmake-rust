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

fn launch(args: &[String]) -> i32 {
    let Some(separator) = args.iter().position(|arg| arg == "--") else {
        eprintln!("No launch/command separator ('--') found!");
        return 1;
    };
    let command = &args[separator + 1..];
    if command.is_empty() || command[0] == ":" {
        return 0;
    }
    let status = std::process::Command::new(&command[0])
        .args(&command[1..])
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status();
    match status {
        Ok(status) => status.code().unwrap_or(1),
        Err(_) => 1,
    }
}

pub fn main() -> i32 {
    let args: Vec<_> = std::env::args_os()
        .skip(1)
        .map(|arg| crate::bytes::decode(arg.as_encoded_bytes()))
        .collect();
    if args.first().is_some_and(|arg| arg == "--launch") {
        return launch(&args[1..]);
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

fn read_custom(dir: &Path, log: &mut Log) -> Vec<String> {
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
    expand_list(&interp.get_safe("CTEST_CUSTOM_TESTS_IGNORE"), false)
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
    let ignore = read_custom(&binary, log);
    if !options.script_quiet {
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
