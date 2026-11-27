use std::collections::BTreeSet;
use std::fmt::Write;
use std::path::Path;

use crate::commands::string::json::{Json, write_json_multiline};
use crate::fileapi::{JsonExt, jarr, jobj, jstr, juint};
use crate::util::expand_list;

use super::Log;
use super::options::{Options, Repeat, integer, number};
use super::process::{ResultData, Status, command_info, timeout_signal};
use super::resources::parse_groups;
use super::testfile::Test;

const SEPARATOR: &str = "----------------------------------------------------------\n";

pub(super) fn clock(format: &str) -> String {
    let format = std::ffi::CString::new(format).unwrap();
    let mut output = [0; 128];
    // SAFETY: the C functions receive valid buffers and localtime_r initializes tm on success.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut local = std::mem::MaybeUninit::uninit();
        if libc::localtime_r(&now, local.as_mut_ptr()).is_null() {
            return String::new();
        }
        let len = libc::strftime(
            output.as_mut_ptr().cast(),
            output.len(),
            format.as_ptr(),
            local.as_ptr(),
        );
        String::from_utf8_lossy(&output[..len]).into_owned()
    }
}

pub(super) fn number_text(value: f64) -> String {
    let mut output = [0; 64];
    // SAFETY: %g expects a double and snprintf writes only within output.
    let len = unsafe {
        libc::snprintf(
            output.as_mut_ptr().cast(),
            output.len(),
            c"%g".as_ptr(),
            value,
        )
    };
    String::from_utf8_lossy(&output[..(len.max(0) as usize).min(output.len() - 1)]).into_owned()
}

pub(super) fn start_log() -> String {
    format!("Start testing: {}\n{SEPARATOR}", clock("%b %d %H:%M %Z"))
}

pub(super) fn append_result(log: &mut String, all_count: usize, test: &Test, result: &ResultData) {
    let ResultData {
        status,
        actual,
        args,
        start,
        diagnostic,
        seconds,
        output,
        reason,
        ..
    } = result;
    let name = &test.name;
    let index = test.index;
    if !diagnostic.is_empty() {
        let _ = writeln!(log, "{diagnostic}");
    }
    let _ = write!(
        log,
        "{index}/{all_count} Testing: {name}\n{index}/{all_count} Test: {name}\nCommand: \"{actual}\""
    );
    for arg in args {
        let _ = write!(log, " \"{arg}\"");
    }
    let raw_output = if start.is_empty() { "" } else { output };
    let _ = write!(
        log,
        "\nDirectory: {}\n\"{name}\" start time: {start}\nOutput:\n{SEPARATOR}{raw_output}<end of output>\nTest time = {seconds:6.2} sec\n{SEPARATOR}",
        test.dir.display()
    );
    let pass = !status.failed() || status.not_run();
    let pass_word = if pass { "Pass" } else { "Fail" };
    if reason.is_empty() {
        let _ = writeln!(log, "Test {pass_word}ed.");
    } else {
        let _ = writeln!(log, "Test {pass_word} Reason:\n{reason}");
    }
    let end = clock("%b %d %H:%M %Z");
    let duration = *seconds as u64;
    let _ = write!(
        log,
        "\"{name}\" end time: {end}\n\"{name}\" time elapsed: {:02}:{:02}:{:02}\n{SEPARATOR}\n",
        duration / 3600,
        duration / 60 % 60,
        duration % 60
    );
}

pub(super) struct Columns {
    pub total: usize,
    pub width: usize,
    pub index_width: usize,
}

impl Columns {
    pub fn new(tests: &[Test], options: &Options) -> Self {
        let total = tests.len();
        let width = tests
            .iter()
            .map(|test| test.name.len())
            .max()
            .unwrap_or(0)
            .max(options.width);
        let index_width = tests.last().map_or(0, |test| test.index).to_string().len();
        Self {
            total,
            width,
            index_width,
        }
    }

    pub fn start(&self, test: &Test, log: &mut Log, completed: usize) {
        if log.progress {
            log.progress(&format!(
                "{}{}\n",
                self.prefix(test, completed, true),
                test.name
            ));
        } else {
            let width = 2 * self.total.to_string().len() + 8;
            log.output(&format!(
                "{:>width$}{:>index_width$}: {}\n",
                "Start ",
                test.index,
                test.name,
                index_width = self.index_width
            ));
        }
    }

    fn prefix(&self, test: &Test, completed: usize, show_count: bool) -> String {
        let total = self.total;
        let width = total.to_string().len();
        let prefix = if show_count {
            format!("{completed:>width$}/{total:>width$} ")
        } else {
            " ".repeat(2 * width.max(2))
        };
        let index = format!(" #{}:", test.index);
        format!(
            "{prefix}Test{:>width$} ",
            index,
            width = self.index_width + 3
        )
    }

    pub fn result(
        &self,
        test: &Test,
        result: &ResultData,
        completed: usize,
        attempt: usize,
        options: &Options,
        log: &mut Log,
    ) {
        let show_count = log.progress
            || match options.repeat {
                Some((Repeat::UntilFail, count)) => attempt == count,
                Some(_) | None => attempt == 1,
            };
        let mut status = match result.status {
            Status::Passed => "   Passed  ".into(),
            Status::Failed => format!("***Failed  {}", result.reason),
            Status::Skipped => "***Skipped ".into(),
            Status::Disabled => "***Not Run (Disabled) ".into(),
            Status::Timeout => result.timeout_signal.as_ref().map_or_else(
                || "***Timeout ".into(),
                |name| format!("***Timeout ({name}) "),
            ),
            Status::Signal(name) => match name {
                "SEGFAULT" => "***Exception: SegFault".into(),
                "ILLEGAL" => "***Exception: Illegal".into(),
                "INTERRUPT" => "***Exception: Interrupt".into(),
                "NUMERICAL" => "***Exception: Numerical".into(),
                _ => format!("{name}\n***Exception: "),
            },
            Status::BadCommand | Status::NotRun => "***Not Run ".into(),
        };
        let _ = writeln!(status, "{:6.2} sec", result.seconds);
        if !log.progress || !matches!(result.status, Status::Passed | Status::Skipped) {
            let gap = usize::from(!log.progress);
            let prefix = self.prefix(test, completed, show_count);
            let dots = ".".repeat(self.width + 4 - gap - test.name.len());
            if log.progress {
                log.progress("\n");
            }
            log.output(&format!(
                "{prefix}{}{}{dots}{status}",
                test.name,
                " ".repeat(gap)
            ));
        }
        if log.progress && completed == self.total {
            log.progress(&format!(
                "{}{}\n",
                self.prefix(test, completed, true),
                test.name
            ));
        }
        if options.output_on_failure
            && result.status.failed()
            && !result.status.not_run()
            && result.status != Status::BadCommand
        {
            log.output(&format!("{}\n", result.output));
        }
    }
}

pub(super) fn show_human(tests: &[Test], options: &Options, log: &mut Log) {
    let columns = Columns::new(tests, options);
    for test in tests {
        command_info(test, &options.config, log);
        if !test.list("LABELS").is_empty() {
            log.verbose(&format!("Labels: {}\n", test.list("LABELS").join(" ")));
        }
        let index = format!(" #{}:", test.index);
        let disabled = if test.enabled("DISABLED") {
            " (Disabled)"
        } else {
            ""
        };
        log.output(&format!(
            "  Test{:>width$} {}{disabled}\n",
            index,
            test.name,
            width = columns.index_width + 3
        ));
    }
    log.output(&format!("\nTotal Tests: {}\n", tests.len()));
}

fn strings(values: &[String]) -> Json {
    Json::Array(values.iter().map(jstr).collect())
}

#[derive(Default)]
struct Backtrace {
    commands: Vec<String>,
    files: Vec<String>,
    nodes: Vec<Json>,
}

impl Backtrace {
    fn add(&mut self, triples: &str) -> Option<usize> {
        if triples.is_empty() {
            return None;
        }
        let triples = expand_list(triples, true);
        if !triples.len().is_multiple_of(3) {
            return None;
        }
        let mut nodes = Vec::new();
        for triple in triples.chunks_exact(3) {
            let mut node = jobj();
            node.set("file", juint(intern(&mut self.files, &triple[0]) as u64));
            let line = integer(&triple[1]);
            if line != 0 {
                node.set("line", Json::Int(line.into()));
            }
            if !triple[2].is_empty() {
                node.set(
                    "command",
                    juint(intern(&mut self.commands, &triple[2]) as u64),
                );
            }
            nodes.push(node);
        }
        let mut parent = None;
        for mut node in nodes.into_iter().rev() {
            if let Some(index) = parent {
                node.set("parent", juint(index as u64));
            }
            parent = Some(self.nodes.len());
            self.nodes.push(node);
        }
        parent
    }

    fn finish(self) -> Json {
        let mut graph = jobj();
        graph.set("commands", strings(&self.commands));
        graph.set("files", strings(&self.files));
        graph.set("nodes", Json::Array(self.nodes));
        graph
    }
}

fn intern(values: &mut Vec<String>, value: &str) -> usize {
    if let Some(index) = values.iter().position(|known| known == value) {
        index
    } else {
        values.push(value.into());
        values.len() - 1
    }
}

fn special_property(name: &str, test: &Test) -> Option<Json> {
    let value = test.property(name);
    match name {
        "ATTACHED_FILES"
        | "ATTACHED_FILES_ON_FAIL"
        | "DEPENDS"
        | "ENVIRONMENT"
        | "ENVIRONMENT_MODIFICATION"
        | "FAIL_REGULAR_EXPRESSION"
        | "FIXTURES_CLEANUP"
        | "FIXTURES_REQUIRED"
        | "FIXTURES_SETUP"
        | "LABELS"
        | "PASS_REGULAR_EXPRESSION"
        | "REQUIRED_FILES"
        | "RESOURCE_LOCK"
        | "SKIP_REGULAR_EXPRESSION" => {
            let values = test.list(name);
            (!values.is_empty()).then(|| strings(values))
        }
        "DISABLED" | "PROCESSOR_AFFINITY" | "RUN_SERIAL" | "WILL_FAIL" => {
            test.enabled(name).then_some(Json::Bool(true))
        }
        "TIMEOUT" => test
            .properties
            .contains_key(name)
            .then(|| Json::Real(number(value))),
        "TIMEOUT_SIGNAL_NAME" => timeout_signal(value).map(|_| jstr(value)),
        "TIMEOUT_SIGNAL_GRACE_PERIOD" => {
            let timeout = number(value);
            (timeout > 0.0 && timeout <= 60.0).then_some(Json::Real(timeout))
        }
        "TIMEOUT_AFTER_MATCH" => {
            let args = expand_list(value, false);
            let [time, patterns] = args.as_slice() else {
                return None;
            };
            let mut object = jobj();
            object.set("timeout", Json::Real(number(time)));
            object.set("regex", strings(&expand_list(patterns, false)));
            Some(object)
        }
        "COST" => {
            let cost = number(value) as f32;
            (cost != 0.0).then_some(Json::Real(cost.into()))
        }
        "SKIP_RETURN_CODE" => {
            let code = integer(value);
            (!value.is_empty() && (0..=255).contains(&code)).then_some(Json::Int(code.into()))
        }
        "PROCESSORS" => (test.processors() != 1).then(|| juint(test.processors() as u64)),
        "RESOURCE_GROUPS" => {
            let groups = parse_groups(value)?;
            (!groups.is_empty()).then(|| {
                Json::Array(
                    groups
                        .into_iter()
                        .map(|requirements| {
                            let mut group = jobj();
                            group.set(
                                "requirements",
                                Json::Array(
                                    requirements
                                        .into_iter()
                                        .map(|(kind, slots)| {
                                            let mut element = jobj();
                                            element.set(".type", jstr(kind));
                                            element.set("slots", juint(slots.into()));
                                            element
                                        })
                                        .collect(),
                                ),
                            );
                            group
                        })
                        .collect(),
                )
            })
        }
        "MEASUREMENT" => (!test.measurements.is_empty()).then(|| {
            Json::Array(
                test.measurements
                    .iter()
                    .map(|(name, value)| {
                        let mut element = jobj();
                        element.set("measurement", jstr(name));
                        element.set("value", jstr(value));
                        element
                    })
                    .collect(),
            )
        }),
        "WORKING_DIRECTORY" => {
            (!test.dir.as_os_str().is_empty()).then(|| jstr(test.dir.to_string_lossy()))
        }
        _ => (!value.is_empty()).then(|| jstr(value)),
    }
}

const PROPERTY_ORDER: &[&str] = &[
    "ATTACHED_FILES_ON_FAIL",
    "ATTACHED_FILES",
    "COST",
    "DEPENDS",
    "DISABLED",
    "ENVIRONMENT",
    "ENVIRONMENT_MODIFICATION",
    "FAIL_REGULAR_EXPRESSION",
    "SKIP_REGULAR_EXPRESSION",
    "FIXTURES_CLEANUP",
    "FIXTURES_REQUIRED",
    "FIXTURES_SETUP",
    "GENERATED_RESOURCE_SPEC_FILE",
    "LABELS",
    "MEASUREMENT",
    "PASS_REGULAR_EXPRESSION",
    "RESOURCE_GROUPS",
    "PROCESSOR_AFFINITY",
    "PROCESSORS",
    "REQUIRED_FILES",
    "RESOURCE_LOCK",
    "RUN_SERIAL",
    "SKIP_RETURN_CODE",
    "TIMEOUT",
    "TIMEOUT_SIGNAL_NAME",
    "TIMEOUT_SIGNAL_GRACE_PERIOD",
    "TIMEOUT_AFTER_MATCH",
    "WILL_FAIL",
    "WORKING_DIRECTORY",
];

pub(super) fn show_json(tests: &[Test], options: &Options, log: &mut Log) {
    let mut backtrace = Backtrace::default();
    let mut entries = jarr();
    for test in tests {
        let actual = command_info(test, &options.config, log);
        if actual == "NOT_AVAILABLE" {
            continue;
        }
        let mut entry = jobj();
        entry.set("name", jstr(&test.name));
        if !options.config.is_empty() {
            entry.set("config", jstr(&options.config));
        }
        if !actual.is_empty() {
            let command = std::iter::once(jstr(actual))
                .chain(test.command[1..].iter().map(jstr))
                .collect();
            entry.set("command", Json::Array(command));
        }
        let mut props = jarr();
        for (key, value) in PROPERTY_ORDER
            .iter()
            .filter_map(|key| special_property(key, test).map(|value| (*key, value)))
            .chain(
                test.properties
                    .iter()
                    .filter(|(key, _)| {
                        key.as_str() != "_BACKTRACE_TRIPLES"
                            && !PROPERTY_ORDER.contains(&key.as_str())
                    })
                    .map(|(key, value)| (key.as_str(), jstr(value))),
            )
        {
            let mut prop = jobj();
            prop.set("name", jstr(key));
            prop.set("value", value);
            props.push(prop);
        }
        if !props.is_empty_container() {
            entry.set("properties", props);
        }
        if let Some(index) = backtrace.add(test.property("_BACKTRACE_TRIPLES")) {
            entry.set("backtrace", juint(index as u64));
        }
        entries.push(entry);
    }
    let mut version = jobj();
    version.set("major", juint(1));
    version.set("minor", juint(0));
    let mut root = jobj();
    root.set("kind", jstr("ctestInfo"));
    root.set("version", version);
    root.set("tests", entries);
    root.set("backtraceGraph", backtrace.finish());
    log.direct(&write_json_multiline(&root));
}

fn xml_escape(value: &str) -> String {
    value
        .chars()
        .map(|c| match c {
            '&' => "&amp;".into(),
            '<' => "&lt;".into(),
            '>' => "&gt;".into(),
            '"' => "&quot;".into(),
            '\t' | '\n' | '\r' => c.to_string(),
            c if c < ' ' => format!("&lt;{}&gt;", c as u32),
            c => c.to_string(),
        })
        .collect()
}

pub(super) fn junit(
    path: &Path,
    tests: &[Test],
    results: &[Option<ResultData>],
    build: &str,
    site: &str,
    date: &str,
    elapsed: f64,
) -> std::io::Result<()> {
    let total = results.iter().flatten().count();
    let count = |check: fn(Status) -> bool| {
        results
            .iter()
            .flatten()
            .filter(|result| check(result.status))
            .count()
    };
    let failed = count(|status| status.failed() && !status.not_run());
    let skipped = count(|status| matches!(status, Status::NotRun | Status::Skipped));
    let disabled = count(|status| status == Status::Disabled);
    let build = if build.is_empty() { "(empty)" } else { build };
    let mut xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuite name=\"{}\"\n\ttests=\"{total}\"\n\tfailures=\"{failed}\"\n\tdisabled=\"{disabled}\"\n\tskipped=\"{skipped}\"\n\thostname=\"{}\"\n\ttime=\"{}\"\n\ttimestamp=\"{date}\"\n\t>\n",
        xml_escape(build),
        xml_escape(site),
        elapsed as u64
    );
    for (test, result) in tests.iter().zip(results) {
        let Some(result) = result else { continue };
        let name = xml_escape(&test.name);
        let status = match result.status {
            Status::Passed => "run",
            Status::Disabled => "disabled",
            Status::NotRun | Status::Skipped => "notrun",
            _ => "fail",
        };
        let _ = writeln!(
            xml,
            "\t<testcase name=\"{name}\" classname=\"{name}\" time=\"{}\" status=\"{status}\">",
            number_text(result.seconds)
        );
        if status == "notrun" {
            let _ = writeln!(
                xml,
                "\t\t<skipped message=\"{}\"/>",
                xml_escape(&result.completion)
            );
        } else if status == "fail" {
            let _ = writeln!(
                xml,
                "\t\t<failure message=\"{}\"/>",
                xml_escape(result.status.description())
            );
        }
        let labels = test.list("LABELS");
        if labels.is_empty() {
            xml.push_str("\t\t<properties/>\n");
        } else {
            let _ = writeln!(
                xml,
                "\t\t<properties>\n\t\t\t<property name=\"cmake_labels\" value=\"{}\"/>\n\t\t</properties>",
                xml_escape(&labels.join(";"))
            );
        }
        let _ = writeln!(
            xml,
            "\t\t<system-out>{}</system-out>\n\t</testcase>",
            xml_escape(&result.output)
        );
    }
    xml.push_str("</testsuite>\n");
    std::fs::write(path, xml)
}

fn xml_content(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn clock_at(format: &str, time: libc::time_t) -> String {
    let format = std::ffi::CString::new(format).unwrap();
    let mut output = [0; 128];
    // SAFETY: the C functions receive valid buffers and localtime_r initializes tm on success.
    unsafe {
        let mut local = std::mem::MaybeUninit::uninit();
        if libc::localtime_r(&time, local.as_mut_ptr()).is_null() {
            return String::new();
        }
        let len = libc::strftime(
            output.as_mut_ptr().cast(),
            output.len(),
            format.as_ptr(),
            local.as_ptr(),
        );
        String::from_utf8_lossy(&output[..len]).into_owned()
    }
}

fn xml_attribute_number(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Number(number) => number.to_string(),
        serde_json::Value::Bool(value) => u8::from(*value).to_string(),
        serde_json::Value::String(value) => xml_escape(value),
        _ => String::new(),
    }
}

fn command_line(result: &ResultData, test: &Test) -> String {
    let actual = if result.actual.is_empty() {
        test.command.first().cloned().unwrap_or_default()
    } else {
        result.actual.clone()
    };
    let args = if result.args.is_empty() && result.actual.is_empty() {
        &test.command[1..]
    } else {
        result.args.as_slice()
    };
    let mut line = actual.replace(' ', "\\ ");
    for arg in args {
        let _ = write!(line, " \"{arg}\"");
    }
    line
}

pub(super) fn test_xml(
    binary: &Path,
    options: &Options,
    tests: &[Test],
    results: &[Option<ResultData>],
    start_epoch: i64,
) -> std::io::Result<()> {
    let info = crate::commands::sysinfo::instrumentation_static_info();
    let value = |key: &str| xml_attribute_number(info.get(key).unwrap_or(&serde_json::Value::Null));
    let end_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64);
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    let _ = writeln!(
        xml,
        "<Site BuildName=\"{}\"\n\tBuildStamp=\"{}-{}\"\n\tName=\"{}\"\n\tGenerator=\"ctest-{}\"\n\tCompilerName=\"\"\n\tCompilerVersion=\"\"\n\tOSName=\"{}\"\n\tHostname=\"{}\"\n\tOSRelease=\"{}\"\n\tOSVersion=\"{}\"\n\tOSPlatform=\"{}\"\n\tIs64Bits=\"{}\"\n\tVendorString=\"{}\"\n\tVendorID=\"{}\"\n\tFamilyID=\"{}\"\n\tModelID=\"{}\"\n\tModelName=\"{}\"\n\tProcessorCacheSize=\"{}\"\n\tNumberOfLogicalCPU=\"{}\"\n\tNumberOfPhysicalCPU=\"{}\"\n\tTotalVirtualMemory=\"{}\"\n\tTotalPhysicalMemory=\"{}\"\n\tLogicalProcessorsPerPhysical=\"1\"\n\tProcessorClockFrequency=\"{}\"\n\t>",
        xml_escape(&options.dashboard_build_name),
        xml_escape(&options.dashboard_tag),
        xml_escape(&options.dashboard_model),
        xml_escape(&options.dashboard_site),
        crate::interp::VERSION_STRING,
        value("OSName"),
        value("hostname"),
        value("OSRelease"),
        value("OSVersion"),
        value("OSPlatform"),
        value("is64Bits"),
        value("vendorString"),
        value("vendorID"),
        value("familyId"),
        value("modelId"),
        value("processorName"),
        value("processorCacheSize"),
        value("numberOfLogicalCPU"),
        value("numberOfPhysicalCPU"),
        value("totalVirtualMemory"),
        value("totalPhysicalMemory"),
        value("processorClockFrequency"),
    );
    xml.push_str("\t<Testing>\n");
    let _ = writeln!(
        xml,
        "\t\t<StartDateTime>{}</StartDateTime>\n\t\t<StartTestTime>{}</StartTestTime>",
        clock_at("%b %d %H:%M %Z", start_epoch),
        start_epoch
    );
    let relative = |test: &Test| {
        test.dir
            .strip_prefix(binary)
            .ok()
            .filter(|path| !path.as_os_str().is_empty())
            .map_or_else(
                || ".".to_owned(),
                |path| path.to_string_lossy().replace('\\', "/"),
            )
    };
    let full_name = |test: &Test| {
        let path = relative(test);
        if path == "." {
            format!("./{}", test.name)
        } else {
            format!("{path}/{}", test.name)
        }
    };
    if results.iter().all(Option::is_none) {
        xml.push_str("\t\t<TestList/>\n");
    } else {
        xml.push_str("\t\t<TestList>\n");
        for (test, result) in tests.iter().zip(results) {
            if result.is_some() {
                let _ = writeln!(xml, "\t\t\t<Test>{}</Test>", xml_escape(&full_name(test)));
            }
        }
        xml.push_str("\t\t</TestList>\n");
    }
    for (test, result) in tests.iter().zip(results) {
        let Some(result) = result else { continue };
        let status = if result.status == Status::Passed {
            "passed"
        } else if result.status.not_run() {
            "notrun"
        } else {
            "failed"
        };
        let command = command_line(result, test);
        let _ = writeln!(
            xml,
            "\t\t<Test Status=\"{status}\">\n\t\t\t<Name>{}</Name>\n\t\t\t<Path>{}</Path>\n\t\t\t<FullName>{}</FullName>\n\t\t\t<FullCommandLine>{}</FullCommandLine>\n\t\t\t<Results>",
            xml_escape(&test.name),
            xml_escape(&relative(test)),
            xml_escape(&full_name(test)),
            xml_content(&command),
        );
        if !result.status.not_run() && (result.status != Status::Passed || result.exit_value != 0) {
            let _ = writeln!(
                xml,
                "\t\t\t\t<NamedMeasurement type=\"text/string\" name=\"Exit Code\">\n\t\t\t\t\t<Value>{}</Value>\n\t\t\t\t</NamedMeasurement>\n\t\t\t\t<NamedMeasurement type=\"text/string\" name=\"Exit Value\">\n\t\t\t\t\t<Value>{}</Value>\n\t\t\t\t</NamedMeasurement>",
                xml_content(result.status.description()),
                result.exit_value,
            );
        }
        if !result.status.not_run() {
            let _ = writeln!(
                xml,
                "\t\t\t\t<NamedMeasurement type=\"numeric/double\" name=\"Execution Time\">\n\t\t\t\t\t<Value>{}</Value>\n\t\t\t\t</NamedMeasurement>",
                number_text(result.seconds),
            );
            if !result.reason.is_empty() {
                let name = if result.status == Status::Passed {
                    "Pass Reason"
                } else {
                    "Fail Reason"
                };
                let _ = writeln!(
                    xml,
                    "\t\t\t\t<NamedMeasurement type=\"text/string\" name=\"{name}\">\n\t\t\t\t\t<Value>{}</Value>\n\t\t\t\t</NamedMeasurement>",
                    xml_content(&result.reason),
                );
            }
        }
        let _ = writeln!(
            xml,
            "\t\t\t\t<NamedMeasurement type=\"numeric/double\" name=\"Processors\">\n\t\t\t\t\t<Value>{}</Value>\n\t\t\t\t</NamedMeasurement>\n\t\t\t\t<NamedMeasurement type=\"text/string\" name=\"Completion Status\">\n\t\t\t\t\t<Value>{}</Value>\n\t\t\t\t</NamedMeasurement>\n\t\t\t\t<NamedMeasurement type=\"text/string\" name=\"Command Line\">\n\t\t\t\t\t<Value>{}</Value>\n\t\t\t\t</NamedMeasurement>\n\t\t\t\t<NamedMeasurement type=\"text/string\" name=\"Environment\">\n\t\t\t\t\t<Value>#CTEST_RESOURCE_GROUP_COUNT=</Value>\n\t\t\t\t</NamedMeasurement>\n\t\t\t\t<Measurement>\n\t\t\t\t\t<Value>{}</Value>\n\t\t\t\t</Measurement>\n\t\t\t</Results>",
            test.processors(),
            xml_escape(&result.completion),
            xml_content(&command),
            xml_content(&result.output),
        );
        let labels: BTreeSet<_> = test.list("LABELS").iter().chain(&result.labels).collect();
        if !labels.is_empty() {
            xml.push_str("\t\t\t<Labels>\n");
            for label in labels {
                let _ = writeln!(xml, "\t\t\t\t<Label>{}</Label>", xml_content(label));
            }
            xml.push_str("\t\t\t</Labels>\n");
        }
        xml.push_str("\t\t</Test>\n");
    }
    let _ = writeln!(
        xml,
        "\t\t<EndDateTime>{}</EndDateTime>\n\t\t<EndTestTime>{}</EndTestTime>\n\t\t<ElapsedMinutes>{}</ElapsedMinutes>\n\t</Testing>\n</Site>",
        clock_at("%b %d %H:%M %Z", end_epoch),
        end_epoch,
        (end_epoch - start_epoch) / 60,
    );
    let directory = binary.join("Testing").join(&options.dashboard_tag);
    std::fs::create_dir_all(&directory)?;
    std::fs::write(directory.join("Test.xml"), xml)
}
