use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::Log;
use super::options::{Options, Repeat, number};
use super::process::{
    Event, Invocation, ResultData, Status, command_info, processors_available, property_errors,
};
use super::report::{self, Columns};
use super::resources::{Allocation, Resources, parse_groups, validate_generated};
use super::testfile::Test;

type Results = Vec<Option<ResultData>>;

struct Cost {
    previous: usize,
    value: f32,
}

#[derive(Default)]
struct CostFile {
    entries: Vec<(String, Cost)>,
    failed: Vec<String>,
}

impl CostFile {
    fn read(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        let mut file = Self::default();
        let mut lines = text.lines();
        for line in lines.by_ref() {
            if line == "---" {
                break;
            }
            let Some((name_and_runs, value)) = line.rsplit_once(' ') else {
                break;
            };
            let Some((name, runs)) = name_and_runs.rsplit_once(' ') else {
                break;
            };
            let (Ok(previous), Ok(value)) = (runs.parse(), value.parse()) else {
                break;
            };
            file.entries.push((name.into(), Cost { previous, value }));
        }
        file.failed = lines
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect();
        file
    }

    fn write(self, path: &Path, tests: &[Test], costs: &[Cost], failed: &[String]) {
        let mut text = String::new();
        let mut retained = BTreeSet::new();
        let mut entry = |name: &str, cost: &Cost| {
            let _ = writeln!(
                text,
                "{name} {} {}",
                cost.previous,
                report::number_text(cost.value.into())
            );
        };
        for (name, old_cost) in self.entries {
            if let Some(index) = tests.iter().rposition(|test| test.name == name) {
                entry(&name, &costs[index]);
                retained.insert(index);
            } else {
                entry(&name, &old_cost);
            }
        }
        for (index, (test, cost)) in tests.iter().zip(costs).enumerate() {
            if !retained.contains(&index) {
                entry(&test.name, cost);
            }
        }
        text.push_str("---\n");
        for name in failed {
            let _ = writeln!(text, "{name}");
        }
        let temp = path.with_extension("txt.tmp");
        if std::fs::write(&temp, text).is_ok() {
            let _ = std::fs::rename(&temp, path);
        }
    }
}

fn dependencies(tests: &[Test], log: &mut Log) -> Result<Vec<BTreeSet<usize>>, ()> {
    let mut names = BTreeMap::new();
    for (index, test) in tests.iter().enumerate() {
        names.entry(&test.name).or_insert(index);
    }
    let dependencies: Vec<BTreeSet<_>> = tests
        .iter()
        .map(|test| {
            test.list("DEPENDS")
                .iter()
                .filter_map(|name| names.get(name).copied())
                .collect()
        })
        .collect();
    log.verbose("Checking test dependency graph...\n");
    for (root, test) in tests.iter().enumerate() {
        let mut visited = BTreeSet::new();
        let mut pending = Vec::from_iter(dependencies[root].iter().copied());
        while let Some(next) = pending.pop() {
            if next == root {
                log.error(&format!("Error: a cycle exists in the test dependency graph for the test \"{}\".\nPlease fix the cycle and run ctest again.\n", test.name));
                return Err(());
            }
            if visited.insert(next) {
                pending.extend(&dependencies[next]);
            }
        }
    }
    log.verbose("Checking test dependency graph end\n");
    Ok(dependencies)
}

pub(super) fn validate(tests: &[Test], options: &Options, log: &mut Log) -> Result<(), ()> {
    let _ = dependencies(tests, log)?;
    load_resources(tests, options, log)?;
    Ok(())
}

fn load_resources(tests: &[Test], options: &Options, log: &mut Log) -> Result<Resources, ()> {
    let explicit_spec = options.resource_spec.as_deref();
    validate_generated(tests, explicit_spec.is_some())
        .map_err(|error| log.error(&format!("{error}\n")))?;
    Ok(explicit_spec
        .map(Resources::read)
        .transpose()
        .map_err(|error| log.error(&error))?
        .unwrap_or_default())
}

fn schedule_order(
    dependencies: &[BTreeSet<usize>],
    tests: &[Test],
    costs: &[Cost],
    previous_failures: &[String],
    jobs: usize,
) -> Vec<usize> {
    let compare = |a: &usize, b: &usize| {
        costs[*b]
            .value
            .partial_cmp(&costs[*a].value)
            .unwrap_or(Ordering::Equal)
    };
    let mut order = Vec::new();
    let mut seen = BTreeSet::new();
    if jobs > 1 {
        let (failed, mut level): (BTreeSet<_>, BTreeSet<_>) =
            (0..tests.len()).partition(|&index| previous_failures.contains(&tests[index].name));
        seen.extend(&failed);
        order.extend(failed);
        let mut levels = Vec::new();
        while !level.is_empty() {
            let deps: BTreeSet<_> = level
                .iter()
                .flat_map(|&index| dependencies[index].iter().copied())
                .collect();
            levels.push(level.difference(&deps).copied().collect::<Vec<_>>());
            level = deps;
        }
        for mut level in levels.into_iter().rev() {
            level.sort_by(compare);
            order.extend(level.into_iter().filter(|&index| seen.insert(index)));
        }
    } else {
        fn visit(
            index: usize,
            dependencies: &[BTreeSet<usize>],
            seen: &mut BTreeSet<usize>,
            order: &mut Vec<usize>,
        ) {
            if seen.insert(index) {
                for &dep in &dependencies[index] {
                    visit(dep, dependencies, seen, order);
                }
                order.push(index);
            }
        }
        let mut pending: Vec<_> = (0..tests.len()).collect();
        pending.sort_by(compare);
        for index in pending {
            visit(index, dependencies, &mut seen, &mut order);
        }
    }
    order
}

struct Running {
    allocation: Allocation,
    affinity: Vec<usize>,
    attempt: usize,
}

struct Session<'a> {
    options: &'a Options,
    log: &'a mut Log,
    last_log: &'a mut String,
    all_count: usize,
    test_timeout: f64,
    test_load: u64,
    fake_load: Option<u64>,
    resources: Resources,
    affinity_free: BTreeSet<usize>,
    columns: Columns,
    completed: usize,
    stop_time_passed: bool,
    failed: Vec<String>,
    sender: mpsc::Sender<Event>,
}

impl Session<'_> {
    fn start(
        &mut self,
        test: &Test,
        allocation: &Allocation,
        affinity: &[usize],
        unavailable: Option<String>,
        previous: Option<&ResultData>,
    ) {
        if previous.is_none() {
            self.log.verbose(&format!("test {}\n", test.index));
        }
        self.columns.start(
            test,
            self.log,
            self.completed + usize::from(previous.is_some()),
        );
        if let Err(result) = self.prepare(test, allocation, affinity, unavailable, previous) {
            let _ = self.sender.send(Event::Done(test.index, result));
        }
    }

    fn prepare(
        &mut self,
        test: &Test,
        allocation: &Allocation,
        affinity: &[usize],
        unavailable: Option<String>,
        previous: Option<&ResultData>,
    ) -> Result<(), Box<ResultData>> {
        let not_run = |completion, diagnostic| {
            Box::new(ResultData::not_started(
                Status::NotRun,
                completion,
                diagnostic,
            ))
        };
        if let Some(message) = unavailable {
            self.log.error(&format!("{message}\n"));
            return Err(not_run("Insufficient resources", message));
        }
        if !test.dir.is_dir() {
            let message = super::directory_error(&test.dir);
            self.log.error(&format!("{message}\n"));
            return Err(not_run("Failed to change working directory", message));
        }
        let invalid = property_errors(test);
        if !invalid.is_empty() {
            self.log.output(&format!("{invalid}\n"));
            return Err(not_run("Invalid Test Properties", invalid));
        }
        if test.enabled("DISABLED") {
            return Err(Box::new(ResultData::not_started(
                Status::Disabled,
                "Disabled",
                String::new(),
            )));
        }
        let failed: Vec<_> = self
            .failed
            .iter()
            .filter(|name| test.fixture_deps.contains(*name))
            .cloned()
            .collect();
        if !failed.is_empty() {
            let message = format!("Failed test dependencies: {}", failed.join(" "));
            self.log.output(&format!("{message}\n"));
            return Err(not_run("Fixture dependency failed", message));
        }
        let exe = command_info(test, &self.options.config, self.log);
        if exe == "NOT_AVAILABLE" {
            let message = if self.options.config.is_empty() {
                "Test not available without configuration.  (Missing \"-C <config>\"?)".into()
            } else {
                format!(
                    "Test not available in configuration \"{}\".",
                    self.options.config
                )
            };
            self.log.error(&format!("{message}\n"));
            let mut result = not_run("Missing Configuration", message);
            result.actual = exe;
            result.args = test.command[1..].to_vec();
            return Err(result);
        }
        for file in test.list("REQUIRED_FILES") {
            if !test.dir.join(file).exists() {
                let message = format!("Unable to find required file: {file}");
                self.log.error(&format!("{message}\n"));
                let mut result = not_run("Required Files Missing", message);
                result.actual = exe;
                result.args = test.command[1..].to_vec();
                return Err(result);
            }
        }
        if exe.is_empty() {
            let message = format!("Unable to find executable: {}", test.command[0]);
            self.log.error(&format!("{message}\n"));
            let mut result = not_run("Unable to find executable", message);
            result.args = test.command[1..].to_vec();
            return Err(result);
        }
        let mut test = test.clone();
        if previous.is_some_and(|previous| previous.alternate_used) {
            test.properties.remove("TIMEOUT_AFTER_MATCH");
        }
        let Some(invocation) = Invocation::prepare(
            &test,
            exe.clone(),
            self.options,
            self.test_timeout,
            &allocation.environment(),
            affinity.to_vec(),
            self.log,
        ) else {
            let mut result =
                ResultData::not_started(Status::BadCommand, "Failed to start", String::new());
            result.start = report::clock("%b %d %H:%M %Z");
            result.actual = exe;
            result.args = test.command[1..].to_vec();
            return Err(Box::new(result));
        };
        invocation.spawn(test, self.sender.clone());
        Ok(())
    }

    fn finish(&mut self, test: &mut Test, result: &mut ResultData, attempt: usize) -> bool {
        let generated = test.property("GENERATED_RESOURCE_SPEC_FILE");
        if !generated.is_empty() {
            match Resources::read(Path::new(generated)) {
                Ok(resources) => {
                    self.resources = resources;
                    self.log
                        .verbose(&format!("Using generated resource spec file {generated}\n"));
                }
                Err(error) => {
                    self.log.error(&error);
                    if !result.status.not_run() && result.status != Status::Timeout {
                        result.status = if test.enabled("WILL_FAIL") {
                            Status::Passed
                        } else {
                            Status::Failed
                        };
                        result.reason = "Invalid resource spec file".into();
                    }
                }
            }
        }
        if !result.labels.is_empty() {
            let labels = test.lists.entry("LABELS".into()).or_default();
            labels.append(&mut result.labels);
            labels.sort();
            labels.dedup();
        }
        self.columns.result(
            test,
            result,
            self.completed + 1,
            attempt,
            self.options,
            self.log,
        );
        report::append_result(self.last_log, self.all_count, test, result);
        if result.stop_time_passed && !self.stop_time_passed {
            self.log
                .error("The stop time has been passed. Stopping all tests.\n");
            self.stop_time_passed = true;
        }
        !self.stop_time_passed
            && self.options.repeat.is_some_and(|(mode, max)| {
                attempt < max
                    && !result.status.not_run()
                    && match mode {
                        Repeat::UntilFail => result.status == Status::Passed,
                        Repeat::UntilPass => {
                            result.status.failed() && result.status != Status::BadCommand
                        }
                        Repeat::AfterTimeout => result.status == Status::Timeout,
                    }
            })
    }
}

pub(super) struct RunContext<'a> {
    pub options: &'a Options,
    pub all_count: usize,
    pub temporary: &'a Path,
    pub config: &'a BTreeMap<String, String>,
}

pub(super) fn run(
    tests: &mut [Test],
    context: &RunContext<'_>,
    log: &mut Log,
    last_log: &mut String,
) -> Result<Results, ()> {
    let RunContext {
        options,
        all_count,
        temporary,
        config,
    } = *context;
    let dependencies = dependencies(tests, log)?;
    let resources = load_resources(tests, options, log)?;
    let jobs = options.jobs();
    let cost_path = config
        .get("CostDataFile")
        .filter(|file| !file.is_empty())
        .map_or_else(
            || temporary.join("CTestCostData.txt"),
            std::path::PathBuf::from,
        );
    let cost_file = CostFile::read(&cost_path);
    let mut costs: Vec<_> = tests
        .iter()
        .map(|test| {
            let mut cost = Cost {
                previous: 0,
                value: number(test.property("COST")) as f32,
            };
            if let Some((_, existing)) = cost_file
                .entries
                .iter()
                .find(|(name, _)| name == &test.name)
            {
                cost.previous = existing.previous;
                if jobs > 1 && cost.value == 0.0 {
                    cost.value = existing.value;
                }
            }
            cost
        })
        .collect();
    if options.random {
        let seed = options.seed.unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as u32
        });
        let _ = writeln!(last_log, "Test order random seed: {seed}");
        // SAFETY: CTest has not launched any test threads or C PRNG users.
        unsafe {
            libc::srand(seed);
            for cost in &mut costs {
                cost.value = libc::rand() as f32;
            }
        }
    }
    let mut pending = schedule_order(&dependencies, tests, &costs, &cost_file.failed, jobs);
    let groups: Vec<_> = tests
        .iter()
        .map(|test| parse_groups(test.property("RESOURCE_GROUPS")).unwrap_or_default())
        .collect();
    let mut running: Vec<Option<Running>> = (0..tests.len()).map(|_| None).collect();
    let mut results: Results = (0..tests.len()).map(|_| None).collect();
    let affinity_total = if tests.iter().any(|test| test.enabled("PROCESSOR_AFFINITY")) {
        processors_available()
    } else {
        BTreeSet::new()
    };
    let affinity_count = affinity_total.len();
    let (sender, receiver) = mpsc::channel();
    let mut session = Session {
        options,
        log,
        last_log,
        all_count,
        test_timeout: config
            .get("TimeOut")
            .map_or(0.0, |value| number(value).trunc()),
        test_load: options
            .test_load
            .or_else(|| {
                config
                    .get("TestLoad")
                    .and_then(|value| value.parse::<u64>().ok())
            })
            .unwrap_or(0),
        fake_load: std::env::var("__CTEST_FAKE_LOAD_AVERAGE_FOR_TESTING")
            .ok()
            .and_then(|value| value.parse::<u64>().ok()),
        resources,
        affinity_free: affinity_total,
        columns: Columns::new(tests, options),
        completed: 0,
        stop_time_passed: false,
        failed: Vec::new(),
        sender,
    };
    let positions: BTreeMap<_, _> = tests
        .iter()
        .enumerate()
        .map(|(pos, test)| (test.index, pos))
        .collect();
    let processors = |test: &Test| {
        let mut processors = test.processors().min(jobs);
        if test.enabled("PROCESSOR_AFFINITY") && affinity_count > 0 {
            processors = processors.min(affinity_count);
        }
        processors
    };
    loop {
        if !session.stop_time_passed && (!options.stop_on_failure || session.failed.is_empty()) {
            let mut occupied = 0usize;
            let mut serial = false;
            let mut locks = BTreeSet::new();
            let mut system_load = 0u64;
            let mut load_slots = u64::MAX;
            if session.test_load > 0 {
                system_load = if let Some(fake) = session.fake_load.take() {
                    fake
                } else if std::env::var("__CTEST_FAKE_LOAD_AVERAGE_FOR_TESTING").is_ok() {
                    1
                } else {
                    let mut load = [0.0];
                    // SAFETY: getloadavg writes one element into load.
                    unsafe {
                        if libc::getloadavg(load.as_mut_ptr(), 1) == 1 {
                            load[0].ceil() as u64
                        } else {
                            0
                        }
                    }
                };
                load_slots = session.test_load.saturating_sub(system_load);
            }
            let mut load_blocked: Vec<(usize, String)> = Vec::new();
            let mut load_started = false;
            for (test, active) in tests.iter().zip(&running) {
                if active.is_some() {
                    occupied = occupied.saturating_add(processors(test));
                    serial |= test.enabled("RUN_SERIAL");
                    locks.extend(test.list("RESOURCE_LOCK").iter().cloned());
                }
            }
            pending.retain(|&position| {
                let test = &tests[position];
                if serial
                    || test.enabled("RUN_SERIAL") && occupied > 0
                    || jobs - occupied < processors(test)
                    || dependencies[position]
                        .iter()
                        .any(|&dependency| results[dependency].is_none())
                    || test
                        .list("RESOURCE_LOCK")
                        .iter()
                        .any(|lock| locks.contains(lock))
                {
                    return true;
                }
                if session.test_load > 0 {
                    if load_slots == 0 {
                        return true;
                    }
                    if processors(test) as u64 > load_slots {
                        load_blocked.push((processors(test), test.name.clone()));
                        return true;
                    }
                }
                let affinity_needed = processors(test);
                let wants_affinity = test.enabled("PROCESSOR_AFFINITY") && affinity_count > 0;
                if wants_affinity && session.affinity_free.len() < affinity_needed {
                    return true;
                }
                let unavailable = session.resources.unavailable(&test.name, &groups[position]);
                let allocation = if unavailable.is_some() {
                    Allocation::default()
                } else if let Some(allocation) = session.resources.allocate(&groups[position]) {
                    allocation
                } else {
                    return true;
                };
                let affinity: Vec<_> = if wants_affinity {
                    session
                        .affinity_free
                        .iter()
                        .take(affinity_needed)
                        .copied()
                        .collect()
                } else {
                    Vec::new()
                };
                for cpu in &affinity {
                    session.affinity_free.remove(cpu);
                }
                load_started = true;
                load_slots = load_slots.saturating_sub(processors(test) as u64);
                occupied += processors(test);
                serial = test.enabled("RUN_SERIAL");
                locks.extend(test.list("RESOURCE_LOCK").iter().cloned());
                session.start(test, &allocation, &affinity, unavailable, None);
                running[position] = Some(Running {
                    allocation,
                    affinity,
                    attempt: 1,
                });
                false
            });
            if session.test_load > 0 && !load_started && !pending.is_empty() {
                if serial {
                    session
                        .log
                        .verbose("***** WAITING, Waiting for RUN_SERIAL test to finish.*****\n");
                } else {
                    let mut message = format!(
                        "***** WAITING, System Load: {system_load}, Max Allowed Load: {}",
                        session.test_load
                    );
                    if let Some((processors, name)) = load_blocked.iter().min() {
                        message.push_str(&format!(", Smallest test {name} requires {processors}"));
                    }
                    message.push_str("*****\n");
                    session.log.verbose(&message);
                }
            }
        }
        if running.iter().all(Option::is_none) {
            if pending.is_empty()
                || session.stop_time_passed
                || (options.stop_on_failure && !session.failed.is_empty())
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("CTest result channel closed"),
            Ok(event) => match event {
                Event::Output(index, line) => session.log.verbose(&format!("{index}: {line}\n")),
                Event::Error(error) => session.log.error(&error),
                Event::Done(index, mut result) => {
                    let position = positions[&index];
                    let active = running[position].as_mut().unwrap();
                    let test = &mut tests[position];
                    let repeat = session.finish(test, &mut result, active.attempt);
                    let cost = &mut costs[position];
                    if result.status == Status::Passed {
                        cost.value =
                            ((cost.previous as f64 * f64::from(cost.value) + result.seconds)
                                / (cost.previous as f64 + 1.0)) as f32;
                        cost.previous += 1;
                    }
                    if repeat {
                        active.attempt += 1;
                        let affinity = active.affinity.clone();
                        session.start(test, &active.allocation, &affinity, None, Some(&result));
                    } else {
                        let finished = running[position].take().unwrap();
                        session.affinity_free.extend(finished.affinity);
                        session.resources.release(&finished.allocation);
                        session.completed += 1;
                        result.completion_order = session.completed;
                        if result.status.failed() {
                            session.failed.push(test.name.clone());
                        }
                        results[position] = Some(*result);
                    }
                }
            },
        }
    }
    cost_file.write(&cost_path, tests, &costs, &session.failed);
    Ok(results)
}

pub(super) fn summarize(
    tests: &[Test],
    results: &Results,
    options: &Options,
    temporary: &Path,
    started: Instant,
    log: &mut Log,
    last_log: &mut String,
) -> bool {
    let failed = results
        .iter()
        .flatten()
        .filter(|result| result.status.failed())
        .count();
    let total = results
        .iter()
        .flatten()
        .filter(|result| result.status != Status::Disabled)
        .count();
    if total == 0 {
        if options.no_tests != "ignore" {
            log.error("No tests were found!!!\n");
        }
        return options.no_tests != "error";
    }
    if options.verbose
        && total > failed
        && (!options.include.is_empty() || !options.exclude.is_empty())
    {
        log.verbose("\nThe following tests passed:\n");
        let mut passed: Vec<_> = tests
            .iter()
            .zip(results)
            .filter_map(|(test, result)| {
                let result = result.as_ref()?;
                matches!(result.status, Status::Passed | Status::Skipped)
                    .then_some((result.completion_order, &test.name))
            })
            .collect();
        passed.sort_unstable();
        for (_, name) in passed {
            log.verbose(&format!("\t{name}\n"));
        }
    }
    let mut percent = (total - failed) as f32 * 100.0 / total as f32;
    if failed > 0 {
        percent = percent.min(99.0);
    }
    log.output(&format!(
        "\n{}% tests passed, {failed} tests failed out of {total}\n",
        percent.round()
    ));
    if !options.no_label_summary {
        let mut labels: BTreeMap<&str, (usize, f64)> = BTreeMap::new();
        for (test, result) in tests.iter().zip(results) {
            for label in test.list("LABELS") {
                let (count, seconds) = labels.entry(label).or_default();
                if let Some(result) = result {
                    *count += 1;
                    *seconds += result.seconds * test.processors() as f64;
                }
            }
        }
        if let Some(width) = labels.keys().map(|label| label.len()).max() {
            log.output("\nLabel Time Summary:\n");
            for (label, (count, seconds)) in labels {
                let plural = if count > 1 { "s" } else { "" };
                let padding = " ".repeat(width + 3 - label.len());
                log.output(&format!(
                    "{label}{padding} = {seconds:6.2} sec*proc ({count} test{plural})\n"
                ));
                let _ = writeln!(last_log, "\n{label} = {seconds:6.2} sec*proc");
            }
            last_log.push('\n');
        }
    }
    log.output(&format!(
        "\nTotal Test time (real) = {:6.2} sec\n",
        started.elapsed().as_secs_f64()
    ));
    let mut disabled_log = String::new();
    let mut failed_log = String::new();
    let mut disabled_text = String::new();
    let mut failed_text = String::new();
    for (test, result) in tests.iter().zip(results) {
        let Some(result) = result else { continue };
        let index = test.index;
        let name = &test.name;
        if result.status.failed() {
            let name_status = format!("{name} ({})", result.status.description());
            let labels = test.list("LABELS");
            let _ = write!(failed_text, "\t{index:3} - {name_status}");
            if !labels.is_empty() {
                let _ = write!(
                    failed_text,
                    "{}{}",
                    " ".repeat(50usize.saturating_sub(name_status.len()).max(1)),
                    labels.join(" ")
                );
            }
            failed_text.push('\n');
            let _ = writeln!(failed_log, "{index}:{name}");
        } else if matches!(result.status, Status::Disabled | Status::Skipped) {
            let reason = if result.status == Status::Disabled {
                "Disabled"
            } else {
                "Skipped"
            };
            let _ = writeln!(disabled_text, "\t{index:3} - {name} ({reason})");
            let _ = writeln!(disabled_log, "{index}:{name}");
        }
    }
    if !disabled_log.is_empty() {
        log.output(&format!(
            "\nThe following tests did not run:\n{disabled_text}"
        ));
        let _ = std::fs::write(temporary.join("LastTestsDisabled.log"), disabled_log);
    }
    if !failed_log.is_empty() {
        log.output(&format!("\nThe following tests FAILED:\n{failed_text}"));
        let _ = std::fs::write(temporary.join("LastTestsFailed.log"), failed_log);
    }
    failed == 0
}
