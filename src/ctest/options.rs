use std::path::PathBuf;

use crate::util::{collapse_full_path, is_on};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Show {
    #[default]
    Run,
    Human,
    Json,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Repeat {
    UntilFail,
    UntilPass,
    AfterTimeout,
}

#[derive(Default)]
pub(super) struct BuildAndTest {
    pub active: bool,
    pub source_dir: String,
    pub binary_dir: String,
    pub targets: Vec<String>,
    pub no_clean: bool,
    pub no_cmake: bool,
    pub two_config: bool,
    pub run_dir: String,
    pub exe_dir: String,
    pub timeout: f64,
    pub generator: String,
    pub platform: String,
    pub toolset: String,
    pub project: String,
    pub makeprogram: String,
    pub config_sample: String,
    pub options: Vec<String>,
    pub test_command: String,
    pub test_args: Vec<String>,
}

#[derive(Default)]
pub(super) struct Options {
    pub dir: Option<PathBuf>,
    pub config: String,
    pub verbose: bool,
    pub quiet: bool,
    pub progress: bool,
    pub show: Show,
    pub print_labels: bool,
    pub no_label_summary: bool,
    pub output_on_failure: bool,
    pub output_log: Option<PathBuf>,
    pub junit: Option<PathBuf>,
    pub include: String,
    pub exclude: String,
    pub include_labels: Vec<String>,
    pub exclude_labels: Vec<String>,
    pub exclude_fixture: String,
    pub exclude_setup: String,
    pub exclude_cleanup: String,
    pub index: String,
    pub union: bool,
    pub rerun_failed: bool,
    pub name_file: Option<PathBuf>,
    pub exclude_name_file: Option<PathBuf>,
    pub parallel: Option<usize>,
    pub test_load: Option<u64>,
    pub timeout: f64,
    pub repeat: Option<(Repeat, usize)>,
    pub random: bool,
    pub seed: Option<u32>,
    pub stop_on_failure: bool,
    pub no_tests: String,
    pub resource_spec: Option<PathBuf>,
    pub width: usize,
    pub interactive_debug: bool,
    pub build_and_test: BuildAndTest,
}

impl Options {
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let mut options = Self {
            width: 30,
            interactive_debug: true,
            output_on_failure: std::env::var("CTEST_OUTPUT_ON_FAILURE")
                .is_ok_and(|value| is_on(&value)),
            config: std::env::var("CMAKE_CONFIG_TYPE").unwrap_or_default(),
            ..Self::default()
        };
        let mut parallel_set = false;
        let mut args = args.iter().peekable();
        while let Some(arg) = args.next() {
            let (flag, attached) = arg
                .split_once('=')
                .map_or((arg.as_str(), None), |(f, v)| (f, Some(v)));
            let mut value = || {
                attached
                    .or_else(|| args.next().map(String::as_str))
                    .ok_or_else(|| {
                        if flag == "--test-dir" || flag == "--repeat-until-fail" {
                            format!("'{flag}' requires an argument")
                        } else {
                            format!("Invalid value used with {flag}")
                        }
                    })
            };
            match flag {
                "--test-dir" => options.dir = Some(value()?.into()),
                "-C" | "--build-config" => {
                    options.config = value()?.rsplit('/').next().unwrap_or_default().into()
                }
                "-V" | "-VV" | "--verbose" | "--extra-verbose" => options.verbose = true,
                "-Q" | "--quiet" => options.quiet = true,
                "--progress" => options.progress = true,
                "-N" => options.show = Show::Human,
                "--show-only" => {
                    options.show = match attached
                        .or_else(|| {
                            if args.peek().is_some_and(|s| !s.starts_with('-')) {
                                args.next().map(String::as_str)
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default()
                    {
                        "" | "human" => Show::Human,
                        "json-v1" => Show::Json,
                        other => {
                            return Err(format!("'--show-only=' given unknown value '{other}'"));
                        }
                    };
                }
                "--print-labels" => options.print_labels = true,
                "--no-label-summary" => options.no_label_summary = true,
                "--no-subproject-summary" | "--show-line-numbers" | "--force-new-ctest-process" => {
                }
                "--output-on-failure" => options.output_on_failure = true,
                "-O" | "--output-log" => {
                    let path = value()?;
                    options.output_log = (!path.is_empty()).then(|| path.into());
                }
                "--output-junit" => {
                    let path = value()?;
                    options.junit = (!path.is_empty()).then(|| path.into());
                }
                "-R" | "--tests-regex" => options.include = value()?.into(),
                "-E" | "--exclude-regex" => options.exclude = value()?.into(),
                "-L" | "--label-regex" => options.include_labels.push(value()?.into()),
                "-LE" | "--label-exclude" => options.exclude_labels.push(value()?.into()),
                "-FA" | "--fixture-exclude-any" => options.exclude_fixture = value()?.into(),
                "-FS" | "--fixture-exclude-setup" => options.exclude_setup = value()?.into(),
                "-FC" | "--fixture-exclude-cleanup" => options.exclude_cleanup = value()?.into(),
                "-I" | "--tests-information" => options.index = value()?.into(),
                "-U" | "--union" => {
                    let _ = value()?;
                    options.union = true;
                }
                "--rerun-failed" => options.rerun_failed = true,
                "--tests-from-file" => options.name_file = Some(value()?.into()),
                "--exclude-from-file" => options.exclude_name_file = Some(value()?.into()),
                "-j" | "--parallel" => {
                    let level = attached.or_else(|| {
                        if args.peek().is_some_and(|s| !s.starts_with('-')) {
                            args.next().map(String::as_str)
                        } else {
                            None
                        }
                    });
                    options.parallel = match level {
                        Some(n) if !n.trim().is_empty() => Some(
                            n.trim()
                                .parse()
                                .map_err(|_| format!("'{flag}' given invalid value '{n}'"))?,
                        ),
                        _ => None,
                    };
                    parallel_set = true;
                }
                "--build-and-test" => {
                    let required = "--build-and-test must have source and binary dir";
                    let (source, binary) = if let Some(attached) = attached {
                        let mut parts = attached.split(';');
                        let source = parts.next().unwrap_or_default().to_owned();
                        let binary = parts.next().unwrap_or_default().to_owned();
                        if parts.next().is_some() || source.is_empty() || binary.is_empty() {
                            return Err(required.into());
                        }
                        (source, binary)
                    } else {
                        let Some(source) = args.next() else {
                            return Err(required.into());
                        };
                        let Some(binary) = args.next() else {
                            return Err(required.into());
                        };
                        (source.clone(), binary.clone())
                    };
                    let cwd = std::env::current_dir().unwrap_or_default();
                    let cwd = cwd.to_string_lossy();
                    let build = &mut options.build_and_test;
                    build.active = true;
                    build.source_dir = collapse_full_path(&source, &cwd);
                    build.binary_dir = collapse_full_path(&binary, &cwd);
                    let _ = std::fs::create_dir_all(&build.binary_dir);
                }
                "--build-target" => options.build_and_test.targets.push(value()?.into()),
                "--build-noclean" => options.build_and_test.no_clean = true,
                "--build-nocmake" => options.build_and_test.no_cmake = true,
                "--build-two-config" => options.build_and_test.two_config = true,
                "--build-run-dir" => options.build_and_test.run_dir = value()?.into(),
                "--build-exe-dir" => options.build_and_test.exe_dir = value()?.into(),
                "--test-timeout" => options.build_and_test.timeout = number(value()?),
                "--build-generator" => options.build_and_test.generator = value()?.into(),
                "--build-generator-platform" => {
                    options.build_and_test.platform = value()?.into();
                }
                "--build-generator-toolset" => options.build_and_test.toolset = value()?.into(),
                "--build-project" => options.build_and_test.project = value()?.into(),
                "--build-makeprogram" => options.build_and_test.makeprogram = value()?.into(),
                "--build-config-sample" => options.build_and_test.config_sample = value()?.into(),
                _ if arg == "--build-options" => {
                    while let Some(next) = args.next() {
                        if next == "--build-target" {
                            if let Some(target) = args.next() {
                                options.build_and_test.targets.push(target.clone());
                            }
                            break;
                        }
                        if next == "--test-command" {
                            if let Some(command) = args.next() {
                                options.build_and_test.test_command = command.clone();
                            }
                            for rest in args.by_ref() {
                                options.build_and_test.test_args.push(rest.clone());
                            }
                            break;
                        }
                        options.build_and_test.options.push(next.clone());
                    }
                }
                _ if arg == "--test-command" => {
                    if args.peek().is_none() {
                        return Err(format!(
                            "Unknown argument: {arg}\nCMake Error: Run 'ctest --help' for all supported options."
                        ));
                    }
                    if let Some(command) = args.next() {
                        options.build_and_test.test_command = command.clone();
                    }
                    for rest in args.by_ref() {
                        options.build_and_test.test_args.push(rest.clone());
                    }
                }
                "--test-load" => {
                    let raw = value()?;
                    match raw.parse() {
                        Ok(load) => options.test_load = Some(load),
                        Err(_) => eprintln!("Invalid value for 'Test Load' : {raw}"),
                    }
                }
                "--timeout" => options.timeout = number(value()?),
                "--repeat" | "--repeat-until-fail" => {
                    if options.repeat.is_some() {
                        return Err("At most one '--repeat' option may be used.".into());
                    }
                    let raw = value()?;
                    let repeat = if flag == "--repeat" {
                        raw.split_once(':')
                            .and_then(|(kind, count)| {
                                let kind = match kind {
                                    "until-fail" => Repeat::UntilFail,
                                    "until-pass" => Repeat::UntilPass,
                                    "after-timeout" => Repeat::AfterTimeout,
                                    _ => return None,
                                };
                                count.parse::<usize>().ok().map(|count| (kind, count))
                            })
                            .ok_or_else(|| format!("'--repeat' given invalid value '{raw}'"))?
                    } else {
                        let count = raw.parse::<i64>().map_err(|_| {
                            format!("'--repeat-until-fail' given non-integer value '{raw}'")
                        })?;
                        (Repeat::UntilFail, count.max(1) as usize)
                    };
                    if repeat.1 > 1 {
                        options.repeat = Some(repeat);
                    }
                }
                "--schedule-random" => options.random = true,
                "--schedule-random-seed" => {
                    let seed = value()?;
                    match seed.parse() {
                        Ok(seed) => options.seed = Some(seed),
                        Err(_) if !options.quiet => {
                            eprintln!("Invalid value for '--schedule-random-seed': {seed}")
                        }
                        Err(_) => {}
                    }
                }
                "--stop-on-failure" => options.stop_on_failure = true,
                "--no-tests" => {
                    let action = value()?;
                    if !matches!(action, "error" | "ignore") {
                        return Err(format!("'--no-tests=' given unknown value '{action}'"));
                    }
                    options.no_tests = action.into();
                }
                "--resource-spec-file" => options.resource_spec = Some(value()?.into()),
                "-W" | "--max-width" => options.width = integer(value()?).max(0) as usize,
                "--interactive-debug-mode" => options.interactive_debug = is_on(value()?),
                _ if arg.starts_with("-j") => {
                    let level = &arg[2..];
                    options.parallel = Some(
                        level
                            .parse()
                            .map_err(|_| format!("'-j' given invalid value '{level}'"))?,
                    );
                    parallel_set = true;
                }
                _ if arg.starts_with('-') => {
                    return Err(format!(
                        "Unknown argument: {arg}\nCMake Error: Run 'ctest --help' for all supported options."
                    ));
                }
                _ => {}
            }
        }
        if !parallel_set {
            options.parallel = match std::env::var("CTEST_PARALLEL_LEVEL") {
                Ok(value) if value.trim().is_empty() => None,
                Ok(value) => Some(value.trim().parse().unwrap_or(1)),
                Err(_) => Some(1),
            };
        }
        if options.no_tests.is_empty() {
            options.no_tests = std::env::var("CTEST_NO_TESTS_ACTION").unwrap_or_default();
        }
        Ok(options)
    }

    pub fn jobs(&self) -> usize {
        match self.parallel {
            Some(0) => usize::MAX,
            Some(jobs) => jobs,
            None => {
                let cpus = match std::env::var("__CTEST_FAKE_PROCESSOR_COUNT_FOR_TESTING") {
                    Ok(fake) => match fake.parse::<usize>() {
                        Ok(fake) => fake,
                        Err(_) => {
                            eprintln!("Failed to parse fake processor count: {fake}");
                            std::thread::available_parallelism().map_or(1, |n| n.get())
                        }
                    },
                    Err(_) => std::thread::available_parallelism().map_or(1, |n| n.get()),
                };
                cpus.max(2)
            }
        }
    }
}

/// Match the C conversion used for numeric test properties.
pub(super) fn number(value: &str) -> f64 {
    std::ffi::CString::new(value.as_bytes())
        .map_or(0.0, |value| unsafe { libc::atof(value.as_ptr()) })
}

pub(super) fn integer(value: &str) -> i32 {
    std::ffi::CString::new(value.as_bytes())
        .map_or(0, |value| unsafe { libc::atoi(value.as_ptr()) })
}
