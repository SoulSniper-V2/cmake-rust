//! Test discovery uses the CMake interpreter with CTest's list-file commands.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::cmregex::Regex;
use crate::configure::{add_cmake_paths, add_default_definitions, find_cmake_root};
use crate::interp::{CmdResult, Interp, Mode, err};
use crate::util::{expand_list, is_on};

use super::options::{Options, integer, number};
use super::{Log, sibling};

#[derive(Clone, Debug)]
pub(super) struct Test {
    pub index: usize,
    pub name: String,
    pub command: Vec<String>,
    pub dir: PathBuf,
    pub properties: BTreeMap<String, String>,
    pub lists: BTreeMap<String, Vec<String>>,
    pub measurements: BTreeMap<String, String>,
    pub fixture_deps: BTreeSet<String>,
    pub property_errors: Vec<String>,
}

impl Test {
    pub fn property(&self, key: &str) -> &str {
        self.properties.get(key).map_or("", String::as_str)
    }

    pub fn enabled(&self, key: &str) -> bool {
        is_on(self.property(key))
    }

    pub fn list(&self, key: &str) -> &[String] {
        self.lists.get(key).map_or(&[], Vec::as_slice)
    }

    pub fn processors(&self) -> usize {
        integer(self.property("PROCESSORS")).max(1) as usize
    }

    fn append_dependency(&mut self, name: &str) {
        let depends = self.lists.entry("DEPENDS".into()).or_default();
        if !depends.iter().any(|dependency| dependency == name) {
            depends.push(name.into());
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct Discovery {
    tests: Vec<Test>,
    directory: PathBuf,
    early_exclude: Option<Regex>,
    ignore: Vec<String>,
}

pub(super) fn interpreter(dir: &Path) -> Interp {
    let dir = dir.to_string_lossy();
    let root = find_cmake_root().unwrap_or_default();
    let mut interp = Interp::new(Mode::Script, &dir, &dir, &root);
    interp.cmake_command = sibling("cmake").to_string_lossy().into_owned();
    add_cmake_paths(&mut interp);
    add_default_definitions(&mut interp);
    for (name, value) in [
        ("CMAKE_SOURCE_DIR", dir.as_ref()),
        ("CMAKE_BINARY_DIR", dir.as_ref()),
        ("CMAKE_CURRENT_SOURCE_DIR", dir.as_ref()),
        ("CMAKE_CURRENT_BINARY_DIR", dir.as_ref()),
    ] {
        interp.set_def(name, value);
    }
    interp
}

pub(super) fn discover(
    dir: &Path,
    options: &mut Options,
    ignore: Vec<String>,
    log: &mut Log,
) -> Result<Vec<Test>, ()> {
    log.verbose("Constructing a list of tests\n");
    let Some(filename) = testfile(dir) else {
        return Ok(Vec::new());
    };
    let mut interp = interpreter(dir);
    interp.set_def("CTEST_CONFIGURATION_TYPE", &options.config);
    interp.ext.ctest_discovery = Some(Discovery {
        directory: dir.into(),
        early_exclude: if options.include.is_empty() && !options.rerun_failed {
            regex(&options.exclude)
        } else {
            None
        },
        ignore,
        ..Discovery::default()
    });
    interp.register("add_test", add_test);
    interp.register("set_tests_properties", set_tests_properties);
    interp.register("set_directory_properties", set_directory_properties);
    interp.register("subdirs", subdirs);
    interp.register("add_subdirectory", add_subdirectory);
    if !interp.read_list_file(&filename.to_string_lossy()) || interp.error || interp.fatal {
        return Err(());
    }
    if options.resource_spec.is_none()
        && let Some(file) = interp
            .get_def("CTEST_RESOURCE_SPEC_FILE")
            .filter(|file| !file.is_empty())
    {
        options.resource_spec = Some(file.into());
    }
    log.verbose("Done constructing a list of tests\n");
    Ok(interp.ext.ctest_discovery.take().unwrap().tests)
}

fn testfile(dir: &Path) -> Option<PathBuf> {
    ["CTestTestfile.cmake", "DartTestfile.txt"]
        .into_iter()
        .map(|file| dir.join(file))
        .find(|path| path.exists())
}

fn add_test(interp: &mut Interp, args: &[String]) -> CmdResult {
    let [name, _, ..] = args else {
        return err("called with incorrect number of arguments");
    };
    let state = interp.ext.ctest_discovery.as_mut().unwrap();
    if matches(&state.early_exclude, name) || state.ignore.contains(name) {
        return Ok(());
    }
    state.tests.push(Test {
        index: state.tests.len() + 1,
        name: name.clone(),
        command: args[1..].to_vec(),
        dir: state.directory.clone(),
        properties: BTreeMap::new(),
        lists: BTreeMap::new(),
        measurements: BTreeMap::new(),
        fixture_deps: BTreeSet::new(),
        property_errors: Vec::new(),
    });
    Ok(())
}

const LIST_PROPERTIES: &[&str] = &[
    "ATTACHED_FILES",
    "ATTACHED_FILES_ON_FAIL",
    "DEPENDS",
    "ENVIRONMENT",
    "ENVIRONMENT_MODIFICATION",
    "FAIL_REGULAR_EXPRESSION",
    "FIXTURES_CLEANUP",
    "FIXTURES_REQUIRED",
    "FIXTURES_SETUP",
    "LABELS",
    "PASS_REGULAR_EXPRESSION",
    "REQUIRED_FILES",
    "RESOURCE_LOCK",
    "SKIP_REGULAR_EXPRESSION",
];

fn add_list(test: &mut Test, key: &str, value: &str) {
    let values = test.lists.entry(key.into()).or_default();
    values.extend(expand_list(value, false));
    if key.starts_with("FIXTURES_") || matches!(key, "LABELS" | "RESOURCE_LOCK") {
        values.sort();
        values.dedup();
    }
}

fn set_tests_properties(interp: &mut Interp, args: &[String]) -> CmdResult {
    let Some(split) = args.iter().position(|arg| arg == "PROPERTIES") else {
        return err("unknown error.");
    };
    let state = interp.ext.ctest_discovery.as_mut().unwrap();
    for pair in args[split + 1..].chunks_exact(2) {
        let [key, value] = pair else { unreachable!() };
        for test in state
            .tests
            .iter_mut()
            .filter(|test| args[..split].contains(&test.name))
        {
            match key.as_str() {
                "WORKING_DIRECTORY" => test.dir = value.into(),
                "MEASUREMENT" => {
                    let (key, value) = value.split_once('=').unwrap_or((value, "1"));
                    test.measurements.insert(key.into(), value.into());
                }
                key if LIST_PROPERTIES.contains(&key) => add_list(test, key, value),
                "TIMEOUT_SIGNAL_NAME" => {
                    if super::process::timeout_signal(value).is_none() {
                        test.property_errors.push(format!(
                            "TIMEOUT_SIGNAL_NAME \"{value}\" not supported on this platform."
                        ));
                    }
                    test.properties.insert(key.clone(), value.clone());
                }
                "TIMEOUT_SIGNAL_GRACE_PERIOD" => {
                    let seconds = number(value);
                    if seconds <= 0.0 {
                        test.property_errors.push(format!(
                            "TIMEOUT_SIGNAL_GRACE_PERIOD \"{value}\" is not greater than \"0\" seconds."
                        ));
                    } else if seconds > 60.0 {
                        test.property_errors.push(format!(
                            "TIMEOUT_SIGNAL_GRACE_PERIOD \"{value}\" is not less than the maximum of \"60\" seconds."
                        ));
                    }
                    test.properties.insert(key.clone(), value.clone());
                }
                "RESOURCE_GROUPS" if super::resources::parse_groups(value).is_none() => {
                    return err("unknown error.");
                }
                _ => {
                    test.properties.insert(key.clone(), value.clone());
                }
            }
        }
    }
    Ok(())
}

fn set_directory_properties(interp: &mut Interp, args: &[String]) -> CmdResult {
    let Some(split) = args.iter().position(|arg| arg == "PROPERTIES") else {
        return err("unknown error.");
    };
    let state = interp.ext.ctest_discovery.as_mut().unwrap();
    for pair in args[split + 1..].chunks_exact(2) {
        if pair[0] == "LABELS" {
            for test in state
                .tests
                .iter_mut()
                .filter(|test| test.dir == state.directory)
            {
                add_list(test, "LABELS", &pair[1]);
            }
        }
    }
    Ok(())
}

fn add_subdirectory(interp: &mut Interp, args: &[String]) -> CmdResult {
    subdirs(interp, &args[..args.len().min(1)])
}

fn subdirs(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.is_empty() {
        return err("called with incorrect number of arguments");
    }
    let parent = interp
        .ext
        .ctest_discovery
        .as_ref()
        .unwrap()
        .directory
        .clone();
    for dir in args {
        let dir = parent.join(dir);
        if !dir.exists() {
            continue;
        }
        let dir = dir.canonicalize().unwrap_or(dir);
        if !dir.is_dir() {
            return err(format!(
                "Failed to change working directory to \"{}\": Not a directory",
                dir.display()
            ));
        }
        if let Some(file) = testfile(&dir) {
            interp.ext.ctest_discovery.as_mut().unwrap().directory = dir;
            let ok = interp.read_dependent_file(&file.to_string_lossy(), false);
            interp.ext.ctest_discovery.as_mut().unwrap().directory = parent.clone();
            if !ok {
                return err(format!("Could not find include file: {}", file.display()));
            }
        }
    }
    Ok(())
}

pub(super) fn regex(value: &str) -> Option<Regex> {
    (!value.is_empty())
        .then(|| Regex::new(value).ok())
        .flatten()
}

pub(super) fn matches(regex: &Option<Regex>, value: &str) -> bool {
    regex
        .as_ref()
        .is_some_and(|regex| regex.find(value).is_some())
}

fn index_set(value: &str, total: usize) -> BTreeSet<usize> {
    if value.is_empty() {
        return BTreeSet::new();
    }
    let from_file = std::fs::read_to_string(value).ok();
    let mut values = from_file.as_deref().unwrap_or(value).trim_end().split(',');
    let mut next = || values.next().filter(|value| !value.is_empty());
    let start = next().map_or(1, integer);
    let end = next().map_or(total as i32, integer);
    let stride = next().map_or(1.0, number);
    let mut indices: BTreeSet<_> = values.map(|v| integer(v) as usize).collect();
    if value.matches(',').count() >= 2 && indices.is_empty() {
        indices.insert(0);
    }
    if stride > 0.0 {
        let start = start as f64;
        let end = end as f64;
        // Multiple fractional positions may select the same integer index.
        let mut position = start;
        while position <= end && position <= total as f64 {
            indices.insert(position as usize);
            let next = position + stride;
            if next <= position {
                break;
            }
            position = next;
        }
    }
    indices
}

fn read_names(path: &Option<PathBuf>, log: &mut Log) -> Result<Option<BTreeSet<String>>, ()> {
    path.as_ref()
        .map(|path| {
            std::fs::read_to_string(path)
                .map(|text| {
                    text.lines()
                        .filter(|name| !name.is_empty())
                        .map(str::to_owned)
                        .collect()
                })
                .map_err(|_| {
                    log.error(&format!(
            "Problem reading test list file: {} while generating list of tests to run.\n",
            path.display()
        ))
                })
        })
        .transpose()
}

pub(super) fn select(
    all: &[Test],
    options: &Options,
    temporary: &Path,
    log: &mut Log,
) -> Result<Vec<Test>, ()> {
    if options.rerun_failed {
        let latest = std::fs::read_dir(temporary)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("LastTestsFailed")
            })
            .max_by_key(|entry| entry.metadata().and_then(|meta| meta.modified()).ok());
        let failed: BTreeSet<usize> = latest
            .and_then(|entry| std::fs::read_to_string(entry.path()).ok())
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.split_once(':'))
            .map(|(index, _)| integer(index) as usize)
            .collect();
        return Ok(all
            .iter()
            .filter(|test| failed.is_empty() || failed.contains(&test.index))
            .cloned()
            .collect());
    }

    let include = regex(&options.include);
    let exclude = regex(&options.exclude);
    let match_labels = |filters: &[String], test: &Test| {
        filters
            .iter()
            .filter(|filter| !filter.is_empty())
            .all(|filter| {
                let pattern = regex(filter);
                test.list("LABELS")
                    .iter()
                    .any(|label| matches(&pattern, label))
            })
    };
    let matched: Vec<bool> = all
        .iter()
        .map(|test| {
            (options.include.is_empty() || matches(&include, &test.name))
                && !matches(&exclude, &test.name)
                && match_labels(&options.include_labels, test)
                && (options.exclude_labels.iter().all(String::is_empty)
                    || !match_labels(&options.exclude_labels, test))
        })
        .collect();
    let indices = index_set(
        &options.index,
        if options.union {
            all.len()
        } else {
            matched.iter().filter(|&&matched| matched).count()
        },
    );
    let names = read_names(&options.name_file, log)?;
    let excluded = read_names(&options.exclude_name_file, log)?;
    let mut matched_index = 0;
    let mut selected: Vec<_> = all
        .iter()
        .zip(matched)
        .filter(|(test, matched)| {
            matched_index += usize::from(*matched);
            let included = if options.union {
                *matched || indices.is_empty() || indices.contains(&test.index)
            } else {
                *matched && (indices.is_empty() || indices.contains(&matched_index))
            };
            included
                && names
                    .as_ref()
                    .is_none_or(|names| names.contains(&test.name))
                && excluded
                    .as_ref()
                    .is_none_or(|names| !names.contains(&test.name))
        })
        .map(|(test, _)| test.clone())
        .collect();

    log.verbose("Updating test list for fixtures\n");
    let added = include_fixtures(&mut selected, all, options, log);
    log.verbose(&format!(
        "Added {added} tests to meet fixture requirements\n"
    ));
    selected.sort_by_key(|test| test.index);
    Ok(selected)
}

fn include_fixtures(
    selected: &mut Vec<Test>,
    all: &[Test],
    options: &Options,
    log: &mut Log,
) -> usize {
    let start_count = selected.len();
    let mut added: BTreeSet<String> = selected.iter().map(|test| test.name.clone()).collect();
    let mut fixtures = BTreeSet::new();
    let mut requirements: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut setups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let exclude_any = regex(&options.exclude_fixture);
    let exclude_setup = regex(&options.exclude_setup);
    let exclude_cleanup = regex(&options.exclude_cleanup);
    let mut index = 0;
    while index < selected.len() {
        if !selected[index].enabled("DISABLED") {
            for fixture in selected[index].list("FIXTURES_REQUIRED").to_vec() {
                requirements
                    .entry(fixture.clone())
                    .or_default()
                    .push(selected[index].name.clone());
                for test in all
                    .iter()
                    .filter(|test| test.list("FIXTURES_SETUP").contains(&fixture))
                {
                    selected[index].fixture_deps.insert(test.name.clone());
                    selected[index].append_dependency(&test.name);
                }
                if fixtures.insert(fixture.clone()) && !matches(&exclude_any, &fixture) {
                    for (kind, excluded, description) in [
                        ("FIXTURES_SETUP", &exclude_setup, "setup"),
                        ("FIXTURES_CLEANUP", &exclude_cleanup, "cleanup"),
                    ] {
                        if !matches(excluded, &fixture) {
                            for test in all.iter().filter(|test| test.list(kind).contains(&fixture))
                            {
                                if added.insert(test.name.clone()) {
                                    selected.push(test.clone());
                                    log.verbose(&format!(
                                        "Added {description} test {} required by fixture {fixture}\n",
                                        test.name
                                    ));
                                }
                            }
                        }
                    }
                }
            }
            for fixture in selected[index].list("FIXTURES_SETUP") {
                setups
                    .entry(fixture.clone())
                    .or_default()
                    .push(selected[index].name.clone());
            }
        }
        index += 1;
    }
    for test in selected.iter_mut() {
        for fixture in test.list("FIXTURES_CLEANUP").to_vec() {
            for dependency in requirements
                .get(&fixture)
                .into_iter()
                .chain(setups.get(&fixture))
                .flatten()
            {
                test.append_dependency(dependency);
            }
        }
    }
    selected.len() - start_count
}
