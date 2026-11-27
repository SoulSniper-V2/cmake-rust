use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::util::{collapse_full_path, is_on};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Show {
    #[default]
    Run,
    Human,
    Json,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Repeat {
    UntilFail,
    UntilPass,
    AfterTimeout,
}

#[derive(Clone, Debug, Default)]
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

#[derive(Clone, Debug, Default)]
pub(super) struct Options {
    pub dir: Option<PathBuf>,
    pub config: String,
    pub verbose: bool,
    pub extra_verbose: bool,
    pub quiet: bool,
    pub debug: bool,
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
    pub scripts: Vec<(PathBuf, bool)>,
    pub definitions: std::collections::BTreeMap<String, String>,
    pub script: bool,
    pub dashboard: Vec<String>,
    pub model: String,
    pub actions: Vec<String>,
    pub dashboard_parts: BTreeSet<String>,
    pub group: String,
    pub dashboard_tag: String,
    pub dashboard_model: String,
    pub dashboard_site: String,
    pub dashboard_build_name: String,
    pub list_presets: bool,
    pub preset_binary_dir: String,
}

fn dashboard_type(value: &str) -> Option<(&'static str, Vec<&'static str>)> {
    for (model, default_parts) in [
        (
            "Experimental",
            vec!["start", "configure", "build", "test", "coverage", "submit"],
        ),
        (
            "Continuous",
            vec![
                "start",
                "update",
                "configure",
                "build",
                "test",
                "coverage",
                "submit",
            ],
        ),
        (
            "Nightly",
            vec![
                "start",
                "update",
                "configure",
                "build",
                "test",
                "coverage",
                "submit",
            ],
        ),
    ] {
        if value == model {
            return Some((model, default_parts));
        }
        for (suffix, part) in [
            ("Start", "start"),
            ("Update", "update"),
            ("Configure", "configure"),
            ("Build", "build"),
            ("Test", "test"),
            ("MemCheck", "memcheck"),
            ("Purify", "memcheck"),
            ("Coverage", "coverage"),
            ("Submit", "submit"),
        ] {
            if value == format!("{model}{suffix}") {
                return Some((model, vec![part]));
            }
        }
    }
    if value == "MemoryCheck" {
        return Some((
            "Experimental",
            vec![
                "start",
                "configure",
                "build",
                "memcheck",
                "coverage",
                "submit",
            ],
        ));
    }
    if value == "NightlyMemoryCheck" {
        return Some((
            "Nightly",
            vec![
                "start",
                "update",
                "configure",
                "build",
                "memcheck",
                "coverage",
                "submit",
            ],
        ));
    }
    None
}

pub(super) const RAW_ERROR: &str = "\u{1f}";

fn dashboard_error(value: &str) -> String {
    format!(
        "{RAW_ERROR}CTest -D called with incorrect option: {value}\nAvailable options are:\n  ctest -D Continuous\n  ctest -D Continuous(Start|Update|Configure|Build)\n  ctest -D Continuous(Test|Coverage|MemCheck|Submit)\n  ctest -D Experimental\n  ctest -D Experimental(Start|Update|Configure|Build)\n  ctest -D Experimental(Test|Coverage|MemCheck|Submit)\n  ctest -D Nightly\n  ctest -D Nightly(Start|Update|Configure|Build)\n  ctest -D Nightly(Test|Coverage|MemCheck|Submit)\n  ctest -D NightlyMemoryCheck"
    )
}

fn valid_dashboard_part(value: &str) -> Option<&'static str> {
    match value.to_ascii_lowercase().as_str() {
        "start" => Some("start"),
        "update" => Some("update"),
        "configure" => Some("configure"),
        "build" => Some("build"),
        "test" => Some("test"),
        "coverage" => Some("coverage"),
        "memcheck" => Some("memcheck"),
        "notes" => Some("notes"),
        "submit" => Some("submit"),
        "extrafiles" => Some("extrafiles"),
        "upload" => Some("upload"),
        "done" => Some("done"),
        _ => None,
    }
}

fn action_error(value: &str) -> String {
    format!(
        "{RAW_ERROR}CTest -T called with incorrect option: {value}\nAvailable options are:\n  ctest -T all\n  ctest -T start\n  ctest -T update\n  ctest -T configure\n  ctest -T build\n  ctest -T test\n  ctest -T coverage\n  ctest -T memcheck\n  ctest -T notes\n  ctest -T submit"
    )
}

fn model_error(value: &str) -> String {
    format!(
        "{RAW_ERROR}CTest -M called with incorrect option: {value}\nAvailable options are:\n  ctest -M Continuous\n  ctest -M Experimental\n  ctest -M Nightly"
    )
}

fn apply_test_preset(
    preset_name: &str,
    list_presets: bool,
    options: &mut Options,
) -> Result<(), String> {
    use crate::presets::{NoTestsAction, RepeatMode, ShowOnly, Verbosity};

    let cwd = std::env::current_dir().unwrap_or_default();
    let home = cwd.to_string_lossy().into_owned();
    let mut graph = crate::presets::Graph::default();
    if !graph.read_project_presets(&home, false) {
        return Err(format!(
            "Could not read presets from {home}:\n{}",
            graph.parse_state.error_message()
        ));
    }
    if list_presets {
        graph.print_test_preset_list(None);
        options.list_presets = true;
        return Ok(());
    }
    let Some(pair) = graph.test_presets.get(preset_name) else {
        graph.print_test_preset_list(None);
        return Err(format!("No such test preset in {home}: \"{preset_name}\""));
    };
    if pair.unexpanded.base.hidden {
        graph.print_test_preset_list(None);
        return Err(format!(
            "Cannot use hidden test preset in {home}: \"{preset_name}\""
        ));
    }
    let Some(preset) = &pair.expanded else {
        graph.print_test_preset_list(None);
        return Err(format!(
            "Could not evaluate test preset \"{preset_name}\": Invalid macro expansion"
        ));
    };
    if !preset.base.condition_result {
        return Err(format!(
            "Cannot use disabled test preset in {home}: \"{preset_name}\""
        ));
    }
    let configure_name = &preset.configure_preset;
    let Some(configure_pair) = graph.configure_presets.get(configure_name) else {
        graph.print_configure_preset_list(&|_| true, None);
        return Err(format!(
            "No such configure preset in {home}: \"{configure_name}\""
        ));
    };
    if configure_pair.unexpanded.base.hidden {
        graph.print_configure_preset_list(&|_| true, None);
        return Err(format!(
            "Cannot use hidden configure preset in {home}: \"{configure_name}\""
        ));
    }
    let Some(configure) = &configure_pair.expanded else {
        return Err(format!(
            "Could not evaluate configure preset \"{configure_name}\": Invalid macro expansion"
        ));
    };
    for (name, value) in &preset.base.environment {
        if let Some(value) = value {
            // SAFETY: preset application is single-threaded before any worker threads exist.
            unsafe { std::env::set_var(name, value) };
        }
    }
    if !preset.configuration.is_empty() {
        options.config = preset.configuration.clone();
    }
    if !configure.binary_dir.is_empty() {
        options.preset_binary_dir = configure.binary_dir.clone();
    }
    if let Some(output) = &preset.output {
        options.progress = output.short_progress.unwrap_or(false);
        match output.verbosity {
            Some(Verbosity::Verbose) => options.verbose = true,
            Some(Verbosity::Extra) => {
                options.verbose = true;
                options.extra_verbose = true;
            }
            _ => {}
        }
        options.output_on_failure = output.output_on_failure.unwrap_or(false);
        options.quiet = output.quiet.unwrap_or(false);
        if !output.output_log_file.is_empty() {
            options.output_log = Some(output.output_log_file.clone().into());
        }
        if !output.output_junit_file.is_empty() {
            options.junit = Some(output.output_junit_file.clone().into());
        }
        options.no_label_summary = !output.label_summary.unwrap_or(true);
        if let Some(width) = output.max_test_name_width {
            options.width = width.max(0) as usize;
        }
    }
    if let Some(filter) = &preset.filter {
        if let Some(include) = &filter.include {
            options.include = include.name.clone();
            if !include.label.is_empty() {
                options.include_labels.push(include.label.clone());
            }
            if let Some(index) = &include.index {
                if index.index_file.is_empty() {
                    let mut text = String::new();
                    if let Some(start) = index.start {
                        text.push_str(&start.to_string());
                    }
                    text.push(',');
                    if let Some(end) = index.end {
                        text.push_str(&end.to_string());
                    }
                    text.push(',');
                    if let Some(stride) = index.stride {
                        text.push_str(&stride.to_string());
                    }
                    text.push(',');
                    text.push_str(
                        &index
                            .specific_tests
                            .iter()
                            .map(i32::to_string)
                            .collect::<Vec<_>>()
                            .join(","),
                    );
                    options.index = text;
                } else {
                    options.index = index.index_file.clone();
                }
            }
            options.union = include.use_union.unwrap_or(false);
        }
        if let Some(exclude) = &filter.exclude {
            options.exclude = exclude.name.clone();
            if !exclude.label.is_empty() {
                options.exclude_labels.push(exclude.label.clone());
            }
            if let Some(fixtures) = &exclude.fixtures {
                options.exclude_fixture = fixtures.any.clone();
                options.exclude_setup = fixtures.setup.clone();
                options.exclude_cleanup = fixtures.cleanup.clone();
            }
        }
    }
    if let Some(execution) = &preset.execution {
        options.stop_on_failure = execution.stop_on_failure.unwrap_or(false);
        if let Some(jobs) = execution.jobs {
            options.parallel = Some(jobs.max(0) as usize);
        }
        if !execution.resource_spec_file.is_empty() {
            options.resource_spec = Some(execution.resource_spec_file.clone().into());
        }
        if let Some(load) = execution.test_load {
            options.test_load = Some(load.max(0) as u64);
        }
        if let Some(show) = execution.show_only {
            options.show = match show {
                ShowOnly::Human => Show::Human,
                ShowOnly::JsonV1 => {
                    options.quiet = true;
                    Show::Json
                }
            };
        }
        if let Some(repeat) = &execution.repeat {
            options.repeat = Some((
                match repeat.mode {
                    RepeatMode::UntilFail => Repeat::UntilFail,
                    RepeatMode::UntilPass => Repeat::UntilPass,
                    RepeatMode::AfterTimeout => Repeat::AfterTimeout,
                },
                repeat.count.max(0) as usize,
            ));
        }
        if let Some(interactive) = execution.interactive_debugging {
            options.interactive_debug = interactive;
        }
        options.random = execution.schedule_random.unwrap_or(false);
        if let Some(timeout) = execution.timeout {
            options.timeout = timeout as f64;
        }
        options.no_tests = match execution.no_tests_action {
            Some(NoTestsAction::Error) => "error".into(),
            Some(NoTestsAction::Ignore) => "ignore".into(),
            _ => options.no_tests.clone(),
        };
    }
    Ok(())
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
        let list_presets = args.iter().any(|arg| arg == "--list-presets");
        let mut preset_name = None;
        for (index, arg) in args.iter().enumerate() {
            if let Some(name) = arg.strip_prefix("--preset=") {
                preset_name = Some(name.to_owned());
                break;
            }
            if arg == "--preset" {
                let Some(name) = args.get(index + 1) else {
                    return Err("'--preset' requires an argument".into());
                };
                preset_name = Some(name.clone());
                break;
            }
        }
        if list_presets || preset_name.is_some() {
            apply_test_preset(
                preset_name.as_deref().unwrap_or(""),
                list_presets,
                &mut options,
            )?;
        }
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
                "-V" | "--verbose" => options.verbose = true,
                "-VV" | "--extra-verbose" => {
                    options.verbose = true;
                    options.extra_verbose = true;
                }
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
                "-S" | "--script" | "-SR" | "--script-run" => {
                    let script = collapse_full_path(
                        value()?,
                        &std::env::current_dir()
                            .unwrap_or_default()
                            .to_string_lossy(),
                    );
                    options.scripts.push((script.into(), true));
                }
                "-SP" | "--script-new-process" => {
                    let script = collapse_full_path(
                        value()?,
                        &std::env::current_dir()
                            .unwrap_or_default()
                            .to_string_lossy(),
                    );
                    options.scripts.push((script.into(), false));
                }
                "-D" | "--dashboard" => {
                    let raw = value()?;
                    if let Some((name, value)) = raw.split_once('=') {
                        let name = name.split_once(':').map_or(name, |(name, _)| name);
                        options.definitions.insert(name.into(), value.into());
                    } else {
                        options.dashboard.push(raw.into());
                    }
                }
                _ if arg.starts_with("-D") && arg.len() > 2 => {
                    let raw = &arg[2..];
                    if let Some((name, value)) = raw.split_once('=') {
                        let name = name.split_once(':').map_or(name, |(name, _)| name);
                        options.definitions.insert(name.into(), value.into());
                    } else {
                        options.dashboard.push(raw.into());
                    }
                }
                "--group" => options.group = value()?.into(),
                "-M" | "--test-model" => options.model = value()?.into(),
                "-T" | "--test-action" => options.actions.push(value()?.into()),
                "--list-presets" => {}
                "--preset" => {
                    let _ = value()?;
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
                "--no-compress-output" => {}
                "--debug" => options.debug = true,
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
        for value in &options.dashboard {
            let Some((model, parts)) = dashboard_type(value) else {
                return Err(dashboard_error(value));
            };
            if options.model.is_empty() {
                options.model = model.into();
            }
            options
                .dashboard_parts
                .extend(parts.into_iter().map(str::to_owned));
        }
        for action in &options.actions {
            if action.eq_ignore_ascii_case("all") {
                for part in [
                    "start",
                    "update",
                    "configure",
                    "build",
                    "test",
                    "coverage",
                    "memcheck",
                    "submit",
                    "notes",
                    "extrafiles",
                    "upload",
                    "done",
                ] {
                    options.dashboard_parts.insert(part.into());
                }
            } else if let Some(part) = valid_dashboard_part(action) {
                options.dashboard_parts.insert(part.into());
            } else {
                return Err(action_error(action));
            }
        }
        if !options.model.is_empty() {
            options.model = match options.model.to_ascii_lowercase().as_str() {
                "experimental" => "Experimental".into(),
                "continuous" => "Continuous".into(),
                "nightly" => "Nightly".into(),
                _ => return Err(model_error(&options.model)),
            };
        } else if !options.dashboard_parts.is_empty() {
            options.model = "Experimental".into();
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
