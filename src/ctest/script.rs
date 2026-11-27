//! CTest dashboard script execution (`ctest -S`).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::interp::{CmdResult, Interp, err};
use crate::util::collapse_full_path;

use super::options::{Options, Repeat, integer};
use super::{Log, Show, execute_detailed, report, testfile};

#[derive(Clone, Debug)]
pub(crate) struct State {
    options: Options,
    source_dir: String,
    binary_dir: String,
    model: String,
    group: String,
    specific_group: String,
    tag: String,
    site: String,
    build_name: String,
    configuration: BTreeMap<String, String>,
    process_steps: bool,
    had_error: bool,
}

impl State {
    fn new(options: &Options) -> Self {
        Self {
            options: options.clone(),
            source_dir: String::new(),
            binary_dir: String::new(),
            model: String::new(),
            group: String::new(),
            specific_group: String::new(),
            tag: String::new(),
            site: hostname(),
            build_name: String::new(),
            configuration: BTreeMap::new(),
            process_steps: false,
            had_error: false,
        }
    }
}

fn hostname() -> String {
    let mut bytes = [0; 256];
    // SAFETY: bytes is a valid writable buffer and len is its size.
    if unsafe { libc::gethostname(bytes.as_mut_ptr().cast(), bytes.len()) } != 0 {
        return String::new();
    }
    let len = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..len]).into_owned()
}

fn append_output_log(path: Option<&Path>, text: &str) {
    if let Some(path) = path {
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = file.write_all(&crate::bytes::encode(text));
        }
    }
}

fn handler_output(interp: &Interp, text: &str, quiet: bool) {
    let state = interp.ext.ctest_script.as_ref().unwrap();
    append_output_log(state.options.output_log.as_deref(), text);
    if state.options.verbose && !state.options.quiet && !quiet {
        print!("{text}");
        let _ = std::io::stdout().flush();
    }
}

fn handler_error(interp: &Interp, text: &str) {
    let state = interp.ext.ctest_script.as_ref().unwrap();
    append_output_log(state.options.output_log.as_deref(), text);
    if !state.options.quiet {
        eprint!("{text}");
    }
}

fn collapse_input(path: &str) -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    collapse_full_path(path, &cwd.to_string_lossy())
}

fn xml_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn base64_upload(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::new();
    for chunk in data.chunks(3) {
        let a = u32::from(chunk[0]);
        let b = u32::from(*chunk.get(1).unwrap_or(&0));
        let c = u32::from(*chunk.get(2).unwrap_or(&0));
        let value = (a << 16) | (b << 8) | c;
        output.push(ALPHABET[(value >> 18) as usize & 63] as char);
        output.push(ALPHABET[(value >> 12) as usize & 63] as char);
        output.push(if chunk.len() > 1 {
            ALPHABET[(value >> 6) as usize & 63] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            ALPHABET[value as usize & 63] as char
        } else {
            '='
        });
    }
    // Match cmCTest::Base64Encode's historic extra padding at a block boundary.
    if data.len().is_multiple_of(3) {
        output.push_str("====");
    }
    output
}

fn handler_binary(interp: &Interp, build: Option<&str>) -> Result<PathBuf, String> {
    let fallback = interp.get_safe("CTEST_BINARY_DIRECTORY");
    let state_binary = interp
        .ext
        .ctest_script
        .as_ref()
        .map(|state| state.binary_dir.clone())
        .unwrap_or_default();
    let binary = build
        .map(str::to_owned)
        .filter(|value| !value.is_empty())
        .or_else(|| (!fallback.is_empty()).then_some(fallback))
        .or_else(|| (!state_binary.is_empty()).then_some(state_binary));
    binary
        .map(|value| PathBuf::from(collapse_input(&value)))
        .ok_or_else(|| {
            "called with no build directory specified.  Either use the BUILD argument or set the CTEST_BINARY_DIRECTORY variable."
                .to_owned()
        })
}

fn parse_return_build(
    args: &[String],
    extra_values: &[&str],
) -> Result<(std::collections::BTreeMap<String, String>, bool), String> {
    let mut values = std::collections::BTreeMap::new();
    let mut quiet = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "QUIET" => quiet = true,
            key @ ("BUILD" | "SOURCE" | "RETURN_VALUE" | "DEFECT_COUNT") => {
                if !extra_values.contains(&key)
                    && !matches!(key, "BUILD" | "SOURCE" | "RETURN_VALUE")
                {
                    return Err(format!("called with unknown argument \"{key}\"."));
                }
                index += 1;
                if index >= args.len() {
                    return Err(format!("{key} argument missing value"));
                }
                values.insert(key.into(), args[index].clone());
            }
            key if extra_values.contains(&key) => {
                index += 1;
                if index >= args.len() {
                    return Err(format!("{key} argument missing value"));
                }
                values.insert(key.into(), args[index].clone());
            }
            value => return Err(format!("called with unknown argument \"{value}\".")),
        }
        index += 1;
    }
    Ok((values, quiet))
}

fn ctest_command_error(interp: &mut Interp, command: &str, message: &str) {
    let text = format!("{command} {message}");
    if interp
        .ext
        .ctest_script
        .as_ref()
        .is_some_and(|state| state.process_steps)
    {
        let frame = crate::messages::Frame {
            file: ".".into(),
            line: 0,
            name: None,
            deferred: false,
        };
        eprintln!(
            "{}",
            crate::messages::render(
                crate::messages::MessageType::FatalError,
                &text,
                &[frame],
                None,
            )
        );
        interp.error = true;
    } else {
        interp.issue_at_unsuppressed_no_top(crate::messages::MessageType::FatalError, &text);
    }
}

fn ctest_update(interp: &mut Interp, args: &[String]) -> CmdResult {
    let (values, quiet) = match parse_return_build(args, &[]) {
        Ok(parsed) => parsed,
        Err(error) => return err(error),
    };
    let source = values
        .get("SOURCE")
        .cloned()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| interp.get_safe("CTEST_SOURCE_DIRECTORY"));
    if source.is_empty() {
        ctest_command_error(
            interp,
            "ctest_update",
            "called with no source directory specified. Use SOURCE argument or set CTEST_SOURCE_DIRECTORY.",
        );
        return Ok(());
    }
    let source_dir = collapse_input(&source);
    let state = interp.ext.ctest_script.as_ref().unwrap();
    let tag = state.tag.clone();
    if tag.is_empty() {
        ctest_command_error(interp, "ctest_update", "called with no current tag.");
        return Ok(());
    }
    let binary = PathBuf::from(if state.binary_dir.is_empty() {
        interp.get_safe("CTEST_BINARY_DIRECTORY")
    } else {
        state.binary_dir.clone()
    });
    let detect = |name: &str| Path::new(&source_dir).join(name).exists();
    let mut kind = if detect("CVS") {
        "CVS"
    } else if detect(".svn") {
        "SVN"
    } else if detect(".bzr") {
        "BZR"
    } else if detect(".git") {
        "GIT"
    } else if detect(".hg") {
        "HG"
    } else if detect(".p4") || detect(".p4config") {
        "P4"
    } else {
        "Unknown"
    };
    let mut command = interp.get_safe("CTEST_UPDATE_COMMAND");
    if kind == "Unknown" && !command.is_empty() {
        let lower = command.to_ascii_lowercase();
        for (needle, name) in [
            ("cvs", "CVS"),
            ("svn", "SVN"),
            ("bzr", "BZR"),
            ("git", "GIT"),
            ("hg", "HG"),
            ("p4", "P4"),
        ] {
            if lower.contains(needle) {
                kind = name;
                break;
            }
        }
    }
    if kind == "Unknown" {
        let lower = interp.get_safe("CTEST_UPDATE_TYPE").to_ascii_lowercase();
        for (needle, name) in [
            ("cvs", "CVS"),
            ("svn", "SVN"),
            ("bzr", "BZR"),
            ("git", "GIT"),
            ("hg", "HG"),
            ("p4", "P4"),
        ] {
            if lower.contains(needle) {
                kind = name;
                break;
            }
        }
    }
    if command.is_empty() {
        let key = match kind {
            "CVS" => "CTEST_CVS_COMMAND",
            "SVN" => "CTEST_SVN_COMMAND",
            "BZR" => "CTEST_BZR_COMMAND",
            "GIT" => "CTEST_GIT_COMMAND",
            "HG" => "CTEST_HG_COMMAND",
            "P4" => "CTEST_P4_COMMAND",
            _ => "",
        };
        if !key.is_empty() {
            command = interp.get_safe(key);
        }
        if command.is_empty() {
            let suffix = if key.is_empty() {
                String::new()
            } else {
                format!(" or {key}")
            };
            ctest_command_error(
                interp,
                "ctest_update",
                &format!(
                    "called with no update command specified. Please set CTEST_UPDATE_COMMAND{suffix}."
                ),
            );
            return Ok(());
        }
    }

    let temporary = binary.join("Testing/Temporary");
    let _ = std::fs::create_dir_all(&temporary);
    let log_path = temporary.join(format!("LastUpdate_{tag}.log"));
    let log = "--- Begin Cleanup ---\n--- End Cleanup ---\n--- Begin Update ---\n--- End Update ---\n--- Begin Revisions ---\n--- End Revisions ---\n";
    if std::fs::write(&log_path, log).is_err() {
        return err(format!("cannot create log file: LastUpdate_{tag}.log"));
    }
    handler_output(
        interp,
        &format!("   Updating {kind} repository: {source_dir}\n"),
        quiet,
    );
    let start = epoch_now();
    let start_date = report::clock("%b %d %H:%M UTC");
    let end = epoch_now();
    let end_date = report::clock("%b %d %H:%M UTC");
    let build_name = interp.get_safe("CTEST_BUILD_NAME");
    let build_name = if build_name.is_empty() {
        "(empty)"
    } else {
        &build_name
    };
    let group = &interp.ext.ctest_script.as_ref().unwrap().group;
    let mut xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Update mode=\"Client\" Generator=\"ctest-{}\">\n\t<Site>{}</Site>\n\t<BuildName>{}</BuildName>\n\t<BuildStamp>{tag}-{group}</BuildStamp>\n\t<StartDateTime>{start_date}</StartDateTime>\n\t<StartTime>{start}</StartTime>\n",
        crate::interp::VERSION_STRING,
        xml_text(&interp.get_safe("CTEST_SITE")),
        xml_text(build_name),
    );
    // The generic unknown-VCS implementation has no tool-specific command line.
    let command_line = if kind == "Unknown" {
        ""
    } else {
        command.as_str()
    };
    xml.push_str(&format!(
        "\t<UpdateCommand>{}</UpdateCommand>\n\t<UpdateType>{kind}</UpdateType>\n",
        xml_text(command_line)
    ));
    let change_id = interp.get_safe("CTEST_CHANGE_ID");
    if !change_id.is_empty() {
        xml.push_str(&format!(
            "\t<ChangeId>{}</ChangeId>\n",
            xml_text(&change_id)
        ));
    }
    xml.push_str(&format!(
        "\t<EndDateTime>{end_date}</EndDateTime>\n\t<EndTime>{end}</EndTime>\n\t<ElapsedMinutes>0</ElapsedMinutes>\n\t<UpdateReturnStatus/>\n</Update>\n"
    ));
    let directory = binary.join("Testing").join(&tag);
    let _ = std::fs::create_dir_all(&directory);
    if std::fs::write(directory.join("Update.xml"), xml).is_err() {
        return err("cannot create resulting XML file: Update.xml");
    }
    if let Some(variable) = values.get("RETURN_VALUE") {
        interp.set_def(variable, "0");
    }
    Ok(())
}

fn ctest_coverage(interp: &mut Interp, args: &[String]) -> CmdResult {
    let (values, quiet) = match parse_return_build(args, &["LABELS"]) {
        Ok(parsed) => parsed,
        Err(error) => return err(error),
    };
    let binary = match handler_binary(interp, values.get("BUILD").map(String::as_str)) {
        Ok(binary) => binary,
        Err(error) => return err(error),
    };
    let state = interp.ext.ctest_script.as_ref().unwrap();
    let tag = state.tag.clone();
    handler_output(interp, "Performing coverage\n", quiet);
    let temporary = binary.join("Testing/Temporary");
    let _ = std::fs::create_dir_all(&temporary);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let _ = std::fs::write(
        temporary.join(format!("LastCoverage_{tag}.log")),
        format!("Performing coverage: {stamp}\n"),
    );
    let mut has_coverage = false;
    for extension in ["gcov", "json", "xml", "cov"] {
        for pattern in [binary.join(format!("**/*.{extension}"))] {
            if pattern.exists() {
                has_coverage = true;
            }
        }
    }
    // Recursive globbing is intentionally conservative here; the common empty
    // dashboard case must not mistake compiler notes for coverage results.
    if !has_coverage && !quiet {
        handler_error(
            interp,
            " Cannot find any coverage files. Ignoring Coverage request.\n",
        );
    }
    if let Some(variable) = values.get("RETURN_VALUE") {
        interp.set_def(variable, "0");
    }
    Ok(())
}

fn sanitizer_defect(line: &str, checker: &str) -> Option<String> {
    match checker {
        "AddressSanitizer" => line
            .split("ERROR: AddressSanitizer: ")
            .nth(1)
            .map(|rest| rest.split(" on").next().unwrap_or(rest).to_owned()),
        "LeakSanitizer" => {
            if line.contains("Direct leak of ") {
                Some("Direct leak".into())
            } else if line.contains("Indirect leak of ") {
                Some("Indirect leak".into())
            } else {
                None
            }
        }
        "ThreadSanitizer" => line
            .split("WARNING: ThreadSanitizer: ")
            .nth(1)
            .map(|rest| {
                rest.split(" (pid=")
                    .next()
                    .unwrap_or(rest)
                    .trim()
                    .to_owned()
            })
            .filter(|defect| !defect.is_empty()),
        "MemorySanitizer" => line
            .split("WARNING: MemorySanitizer: ")
            .nth(1)
            .map(|rest| rest.trim().to_owned())
            .filter(|defect| !defect.is_empty()),
        "UndefinedBehaviorSanitizer" => line
            .split("runtime error: ")
            .nth(1)
            .map(|rest| rest.trim().to_owned())
            .filter(|defect| !defect.is_empty()),
        _ => None,
    }
}

fn postprocess_sanitizer_logs(
    interp: &Interp,
    temporary: &Path,
    checker: &str,
    quiet: bool,
) -> usize {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(temporary) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("MemoryChecker.") && name.contains(".log.") {
                files.push(entry.path());
            }
        }
    }
    files.sort();
    let mut order = Vec::<String>::new();
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    let mut total = 0;
    let mut per_test = Vec::<(usize, usize)>::new();
    for file in files {
        let contents = std::fs::read_to_string(&file).unwrap_or_default();
        let mut test_defects = 0;
        for line in contents.lines() {
            if let Some(defect) = sanitizer_defect(line, checker) {
                if !counts.contains_key(&defect) {
                    order.push(defect.clone());
                }
                *counts.entry(defect).or_insert(0) += 1;
                test_defects += 1;
                total += 1;
            }
        }
        let test_number = file
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| {
                name.strip_prefix("MemoryChecker.")?
                    .split('.')
                    .next()?
                    .parse::<usize>()
                    .ok()
            })
            .unwrap_or(0);
        per_test.push((test_number, test_defects));
        if let Some(name) = file.file_name().and_then(|name| name.to_str())
            && let Some((base, _pid)) = name.rsplit_once('.')
        {
            let target = file.with_file_name(base);
            let source = file.display().to_string();
            if std::fs::rename(&file, &target).is_ok() {
                let verbose = interp
                    .ext
                    .ctest_script
                    .as_ref()
                    .is_some_and(|state| state.options.verbose);
                if verbose {
                    handler_output(
                        interp,
                        &format!("Renaming: {source} to: {}\n", target.display()),
                        quiet,
                    );
                }
            }
        }
    }
    handler_output(interp, "-- Processing memory checking output:\n", quiet);
    for (test, defects) in per_test {
        if defects > 0 {
            handler_output(
                interp,
                &format!(
                    "1/1 MemCheck: #{test}: TestSan ..........................   Defects: {defects}\n"
                ),
                quiet,
            );
        }
    }
    handler_output(
        interp,
        "MemCheck log files can be found here: (<#> corresponds to test number)\n",
        quiet,
    );
    handler_output(
        interp,
        &format!("{}/MemoryChecker.<#>.log\n", temporary.display()),
        quiet,
    );
    handler_output(interp, "Memory checking results:\n", quiet);
    for defect in order {
        handler_output(
            interp,
            &format!("{} - {}\n", defect, counts[&defect]),
            quiet,
        );
    }
    total
}

fn ctest_memcheck(interp: &mut Interp, args: &[String]) -> CmdResult {
    let (values, quiet) = match parse_return_build(args, &["DEFECT_COUNT"]) {
        Ok(parsed) => parsed,
        Err(error) => return err(error),
    };
    let binary = match handler_binary(interp, values.get("BUILD").map(String::as_str)) {
        Ok(binary) => binary,
        Err(error) => return err(error),
    };
    handler_output(
        interp,
        &format!("Memory check project {}\n", binary.display()),
        quiet,
    );
    let checker = interp.get_safe("CTEST_MEMORYCHECK_COMMAND");
    let checker_type = interp.get_safe("CTEST_MEMORYCHECK_TYPE");
    let sanitizer = matches!(
        checker_type.as_str(),
        "AddressSanitizer"
            | "LeakSanitizer"
            | "ThreadSanitizer"
            | "MemorySanitizer"
            | "UndefinedBehaviorSanitizer"
    );
    let valid = sanitizer || (!checker.is_empty() && Path::new(&checker).is_file());
    if !valid {
        handler_error(
            interp,
            "Memory checker (MemoryCheckCommand) not set, or cannot find the specified program.\n",
        );
        if let Some(variable) = values.get("RETURN_VALUE") {
            interp.set_def(variable, "-1");
        }
        if let Some(variable) = values.get("DEFECT_COUNT") {
            interp.set_def(variable, "0");
        }
        return Ok(());
    }
    if !sanitizer {
        if let Some(variable) = values.get("RETURN_VALUE") {
            interp.set_def(variable, "-1");
        }
        if let Some(variable) = values.get("DEFECT_COUNT") {
            interp.set_def(variable, "0");
        }
        return Ok(());
    }

    if interp
        .ext
        .ctest_script
        .as_ref()
        .is_some_and(|state| state.options.extra_verbose)
    {
        handler_output(
            interp,
            &format!(
                "SetCTestConfigurationFromCMakeVariable:MemoryCheckType:CTEST_MEMORYCHECK_TYPE\nSetCTestConfiguration:MemoryCheckType:{checker_type}\n"
            ),
            quiet,
        );
        let sanitizer_options = interp.get_safe("CTEST_MEMORYCHECK_SANITIZER_OPTIONS");
        if !sanitizer_options.is_empty() {
            handler_output(
                interp,
                &format!(
                    "SetCTestConfigurationFromCMakeVariable:MemoryCheckSanitizerOptions:CTEST_MEMORYCHECK_SANITIZER_OPTIONS\nSetCTestConfiguration:MemoryCheckSanitizerOptions:{sanitizer_options}\n"
                ),
                quiet,
            );
        }
    }

    let temporary = binary.join("Testing/Temporary");
    let _ = std::fs::create_dir_all(&temporary);
    let mut options = test_options(interp, &binary);
    options.memcheck_type = checker_type.clone();
    options.memcheck_sanitizer_options = interp.get_safe("CTEST_MEMORYCHECK_SANITIZER_OPTIONS");
    options.memcheck_log_dir = temporary.to_string_lossy().into_owned();
    options.memcheck_cmake_command = interp.cmake_command.clone();
    let handler_verbose = interp
        .ext
        .ctest_script
        .as_ref()
        .is_some_and(|state| state.options.verbose);
    let mut log = Log::new(&options);
    log.suppress_output = !handler_verbose || quiet;
    let old_dir = std::env::current_dir().unwrap_or_default();
    let (code, no_tests) = execute_detailed(binary.clone(), options, &mut log);
    let _ = std::env::set_current_dir(old_dir);
    let defects = postprocess_sanitizer_logs(interp, &temporary, &checker_type, quiet);
    let result_code = if no_tests && code == 8 { 0 } else { code };
    if let Some(variable) = values.get("RETURN_VALUE") {
        interp.set_def(variable, if result_code == 0 { "0" } else { "-1" });
    }
    if let Some(variable) = values.get("DEFECT_COUNT") {
        interp.set_def(variable, defects.to_string());
    }
    Ok(())
}

fn parse_upload_args(args: &[String]) -> Result<(Vec<String>, bool), String> {
    let mut files = Vec::new();
    let mut quiet = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "QUIET" => quiet = true,
            "FILES" => {
                index += 1;
                while index < args.len() && args[index] != "QUIET" {
                    files.push(args[index].clone());
                    index += 1;
                }
                continue;
            }
            value => return Err(format!("called with unknown argument \"{value}\".")),
        }
        index += 1;
    }
    files.sort();
    files.dedup();
    Ok((files, quiet))
}

fn ctest_upload(interp: &mut Interp, args: &[String]) -> CmdResult {
    let (files, quiet) = match parse_upload_args(args) {
        Ok(parsed) => parsed,
        Err(error) => return err(error),
    };
    for file in &files {
        if !Path::new(file).is_file() {
            return err(format!(
                "File \"{file}\" does not exist. Cannot submit a non-existent file."
            ));
        }
    }
    let state = interp.ext.ctest_script.as_ref().unwrap();
    let binary = PathBuf::from(&state.binary_dir);
    let tag = state.tag.clone();
    let group = state.group.clone();
    let build_name = interp.get_safe("CTEST_BUILD_NAME");
    let build_name = if build_name.is_empty() {
        "(empty)"
    } else {
        &build_name
    };
    let mut xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<?xml-stylesheet type=\"text/xsl\" href=\"Dart/Source/Server/XSL/Build.xsl <file:///Dart/Source/Server/XSL/Build.xsl> \"?>\n<Site BuildName=\"{}\" BuildStamp=\"{tag}-{group}\" Name=\"{}\" Generator=\"ctest-{}\">\n\t<Upload>\n\t\t<Time>{}</Time>\n",
        xml_text(build_name),
        xml_text(&interp.get_safe("CTEST_SITE")),
        crate::interp::VERSION_STRING,
        epoch_now(),
    );
    for file in &files {
        handler_output(interp, &format!("\tUpload file: {file}\n"), quiet);
        let data = std::fs::read(file).unwrap_or_default();
        xml.push_str(&format!(
            "\t\t<File filename=\"{}\">\n\t\t\t<Content encoding=\"base64\">{}</Content>\n\t\t</File>\n",
            xml_text(file),
            base64_upload(&data),
        ));
    }
    xml.push_str("\t</Upload>\n</Site>\n");
    let directory = binary.join("Testing").join(&tag);
    let _ = std::fs::create_dir_all(&directory);
    if std::fs::write(directory.join("Upload.xml"), xml).is_err() {
        handler_error(interp, "Cannot open Upload.xml file\n");
        interp.ext.ctest_script.as_mut().unwrap().had_error = true;
    }
    Ok(())
}

fn submit_url(interp: &Interp, explicit: Option<&str>) -> String {
    if let Some(url) = explicit.filter(|value| !value.is_empty()) {
        return url.to_owned();
    }
    let direct = interp.get_safe("CTEST_SUBMIT_URL");
    if !direct.is_empty() {
        return direct;
    }
    let method = interp.get_safe("CTEST_DROP_METHOD");
    let method = if method.is_empty() { "http" } else { &method };
    let user = interp.get_safe("CTEST_DROP_SITE_USER");
    let password = interp.get_safe("CTEST_DROP_SITE_PASSWORD");
    let site = interp.get_safe("CTEST_DROP_SITE");
    let location = interp.get_safe("CTEST_DROP_LOCATION");
    let mut url = format!("{method}://");
    if !user.is_empty() {
        url.push_str(&user);
        if !password.is_empty() {
            let _ = std::fmt::Write::write_fmt(&mut url, format_args!(":{password}"));
        }
        url.push('@');
    }
    url.push_str(&site);
    url.push_str(&location);
    url
}

fn submit_error_for(url: &str) -> String {
    let after_scheme = url.split("://").nth(1).unwrap_or_default();
    let authority = after_scheme.split('/').next().unwrap_or_default();
    let hostport = authority.rsplit('@').next().unwrap_or_default();
    if hostport.is_empty() {
        return "URL rejected: No host part in the URL".into();
    }
    let (host, port) = match hostport.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => {
            (host.to_owned(), port.to_owned())
        }
        _ => (
            hostport.to_owned(),
            if url.starts_with("https://") {
                "443"
            } else {
                "80"
            }
            .into(),
        ),
    };
    if host == "localhost" || host == "127.0.0.1" || host == "::1" {
        format!("Failed to connect to {host} port {port} after 0 ms: Could not connect to server")
    } else {
        format!("Could not resolve host: {host}")
    }
}

#[derive(Default)]
struct SubmitArgs {
    parts: Option<Vec<String>>,
    files: Option<Vec<String>>,
    return_value: Option<String>,
    build_id: Option<String>,
    headers: Vec<String>,
    retry_count: String,
    retry_delay: String,
    submit_url: Option<String>,
    quiet: bool,
    internal_test: bool,
    cdash_upload: bool,
    cdash_file: String,
    cdash_type: String,
}

fn submit_missing_value(keyword: &str) -> String {
    format!("Error after keyword \"{keyword}\":\n  missing required value")
}

fn parse_submit_args(args: &[String]) -> Result<SubmitArgs, String> {
    let mut parsed = SubmitArgs {
        cdash_upload: args.first().is_some_and(|arg| arg == "CDASH_UPLOAD"),
        ..SubmitArgs::default()
    };
    let keywords: &[&str] = if parsed.cdash_upload {
        &[
            "CDASH_UPLOAD",
            "CDASH_UPLOAD_TYPE",
            "RETURN_VALUE",
            "BUILD_ID",
            "HTTPHEADER",
            "RETRY_COUNT",
            "RETRY_DELAY",
            "SUBMIT_URL",
            "QUIET",
            "INTERNAL_TEST_CHECKSUM",
        ]
    } else {
        &[
            "PARTS",
            "FILES",
            "RETURN_VALUE",
            "BUILD_ID",
            "HTTPHEADER",
            "RETRY_COUNT",
            "RETRY_DELAY",
            "SUBMIT_URL",
            "QUIET",
            "INTERNAL_TEST_CHECKSUM",
        ]
    };
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "QUIET" => {
                parsed.quiet = true;
                index += 1;
            }
            "INTERNAL_TEST_CHECKSUM" => {
                parsed.internal_test = true;
                index += 1;
            }
            key @ ("RETURN_VALUE" | "BUILD_ID" | "RETRY_COUNT" | "RETRY_DELAY" | "SUBMIT_URL") => {
                index += 1;
                if index >= args.len() {
                    return Err(submit_missing_value(key));
                }
                let value = args[index].clone();
                match key {
                    "RETURN_VALUE" => {
                        if parsed.return_value.is_some() {
                            return Err("called with more than one value for RETURN_VALUE".into());
                        }
                        parsed.return_value = Some(value);
                    }
                    "BUILD_ID" => parsed.build_id = Some(value),
                    "RETRY_COUNT" => parsed.retry_count = value,
                    "RETRY_DELAY" => parsed.retry_delay = value,
                    "SUBMIT_URL" => parsed.submit_url = Some(value),
                    _ => {}
                }
                index += 1;
            }
            "CDASH_UPLOAD" if parsed.cdash_upload && index == 0 => {
                index += 1;
                if index >= args.len() || keywords.contains(&args[index].as_str()) {
                    return Err(submit_missing_value("CDASH_UPLOAD"));
                }
                parsed.cdash_file = args[index].clone();
                index += 1;
            }
            "CDASH_UPLOAD_TYPE" if parsed.cdash_upload => {
                index += 1;
                if index >= args.len() || keywords.contains(&args[index].as_str()) {
                    return Err(submit_missing_value("CDASH_UPLOAD_TYPE"));
                }
                parsed.cdash_type = args[index].clone();
                index += 1;
            }
            key @ ("PARTS" | "FILES" | "HTTPHEADER")
                if !parsed.cdash_upload || key == "HTTPHEADER" =>
            {
                index += 1;
                let mut values = Vec::new();
                while index < args.len() && !keywords.contains(&args[index].as_str()) {
                    values.push(args[index].clone());
                    index += 1;
                }
                match key {
                    "PARTS" => parsed.parts = Some(values),
                    "FILES" => parsed.files = Some(values),
                    "HTTPHEADER" => parsed.headers.extend(values),
                    _ => {}
                }
            }
            value => return Err(format!("called with unknown argument \"{value}\".")),
        }
    }
    Ok(parsed)
}

fn valid_submit_part(part: &str) -> bool {
    matches!(
        part.to_ascii_lowercase().as_str(),
        "start"
            | "update"
            | "configure"
            | "build"
            | "test"
            | "coverage"
            | "memcheck"
            | "notes"
            | "submit"
            | "extrafiles"
            | "upload"
            | "done"
    )
}

fn submit_part_file(part: &str) -> Option<&'static str> {
    match part.to_ascii_lowercase().as_str() {
        "update" => Some("Update.xml"),
        "configure" => Some("Configure.xml"),
        "build" => Some("Build.xml"),
        "test" => Some("Test.xml"),
        "coverage" => Some("Coverage.xml"),
        "memcheck" => Some("DynamicAnalysis.xml"),
        "notes" => Some("Notes.xml"),
        "upload" => Some("Upload.xml"),
        _ => None,
    }
}

fn md5_file(path: &Path, internal_test: bool) -> String {
    if internal_test {
        return "ffffffffffffffffffffffffffffffff".into();
    }
    std::process::Command::new("md5sum")
        .arg(path)
        .output()
        .ok()
        .and_then(|output| {
            String::from_utf8(output.stdout)
                .ok()
                .and_then(|text| text.split_whitespace().next().map(str::to_owned))
        })
        .unwrap_or_default()
}

fn cdash_submit(interp: &mut Interp, parsed: &SubmitArgs) -> CmdResult {
    let file = collapse_input(&parsed.cdash_file);
    if !Path::new(&file).is_file() {
        handler_error(
            interp,
            &format!("Upload file not found: '{}'\n", parsed.cdash_file),
        );
        if let Some(variable) = &parsed.return_value {
            interp.set_def(variable, "-1");
        }
        interp.ext.ctest_script.as_mut().unwrap().had_error = true;
        return Ok(());
    }
    let url = submit_url(interp, parsed.submit_url.as_deref());
    if !url.starts_with("http://") && !url.starts_with("https://") {
        handler_error(
            interp,
            "Only http and https are supported for CDASH_UPLOAD\n",
        );
        if let Some(variable) = &parsed.return_value {
            interp.set_def(variable, "-1");
        }
        interp.ext.ctest_script.as_mut().unwrap().had_error = true;
        return Ok(());
    }
    let debug = interp
        .ext
        .ctest_script
        .as_ref()
        .is_some_and(|state| state.options.debug);
    if debug {
        for header in &parsed.headers {
            handler_output(
                interp,
                &format!("Add HTTP Header: \"{header}\"\n"),
                parsed.quiet,
            );
        }
    }
    let retry_count = parsed.retry_count.parse::<usize>().unwrap_or(0);
    let retry_delay = parsed.retry_delay.parse::<u64>().unwrap_or(0);
    if parsed.internal_test && retry_count > 0 {
        for attempt in 1..=retry_count {
            handler_output(
                interp,
                &format!(
                    "   Upload failed, waiting {retry_delay} seconds...\n   Retry upload: Attempt {attempt} of {retry_count}\n"
                ),
                parsed.quiet,
            );
        }
        handler_error(interp, "error uploading to CDash.\n");
    } else {
        handler_error(interp, "Error in HttpRequest\n");
    }
    if let Some(variable) = &parsed.return_value {
        interp.set_def(variable, "-1");
    }
    interp.ext.ctest_script.as_mut().unwrap().had_error = true;
    Ok(())
}

fn ctest_submit(interp: &mut Interp, args: &[String]) -> CmdResult {
    let mut parsed = match parse_submit_args(args) {
        Ok(parsed) => parsed,
        Err(error) => {
            if error.starts_with("Error after keyword") {
                interp.issue(crate::messages::MessageType::FatalError, &error);
                return Ok(());
            }
            return err(error);
        }
    };
    if let Some(parts) = &mut parsed.parts {
        parts.retain(|part| {
            if valid_submit_part(part) {
                true
            } else {
                interp.issue(
                    crate::messages::MessageType::FatalError,
                    &format!("Part name \"{part}\" is invalid."),
                );
                false
            }
        });
    }
    if let Some(files) = &mut parsed.files {
        files.retain(|file| {
            if Path::new(file).is_file() {
                true
            } else {
                interp.issue(
                    crate::messages::MessageType::FatalError,
                    &format!("File \"{file}\" does not exist. Cannot submit a non-existent file."),
                );
                false
            }
        });
    }
    if parsed.cdash_upload {
        return cdash_submit(interp, &parsed);
    }

    let state = interp.ext.ctest_script.as_ref().unwrap();
    let binary = PathBuf::from(&state.binary_dir);
    let tag = state.tag.clone();
    let group = if state.group.is_empty() {
        state.model.clone()
    } else {
        state.group.clone()
    };
    let specific_group = state.specific_group.clone();
    let debug = state.options.debug;
    let directory = binary.join("Testing").join(&tag);
    let _ = std::fs::create_dir_all(&directory);
    let temporary = binary.join("Testing/Temporary");
    let _ = std::fs::create_dir_all(&temporary);

    let default_parts = [
        "update",
        "configure",
        "build",
        "test",
        "coverage",
        "memcheck",
        "notes",
        "upload",
        "done",
    ];
    let selected_parts: Vec<String> = if let Some(parts) = &parsed.parts {
        parts.iter().map(|part| part.to_ascii_lowercase()).collect()
    } else if parsed.files.is_some() {
        Vec::new()
    } else {
        default_parts.iter().map(|part| part.to_string()).collect()
    };

    let mut files = Vec::new();
    if let Some(extra) = &parsed.files {
        let mut extra = extra.clone();
        extra.sort();
        extra.dedup();
        files.extend(extra);
    }
    for part in &selected_parts {
        if let Some(name) = submit_part_file(part)
            && directory.join(name).is_file()
            && !files.iter().any(|file| file == name)
        {
            files.push(name.to_owned());
        }
    }
    if selected_parts.iter().any(|part| part == "done") {
        files.push("Done.xml".into());
    }

    let mut log = String::from("Upload files:\n");
    for (index, file) in files.iter().enumerate() {
        let _ = std::fmt::Write::write_fmt(&mut log, format_args!("{index}\t{file}\n"));
    }
    let extra_verbose = interp
        .ext
        .ctest_script
        .as_ref()
        .is_some_and(|state| state.options.extra_verbose);
    if extra_verbose {
        let mut log_configuration = |key: &str, ctest_variable: &str, cmake_variable: &str| {
            let ctest_value = interp.get_safe(ctest_variable);
            let cmake_value = interp.get_safe(cmake_variable);
            let env_value = std::env::var(cmake_variable).unwrap_or_default();
            let (value, first) = if !ctest_value.is_empty() {
                (
                    ctest_value,
                    format!("SetCTestConfigurationFromCMakeVariable:{key}:{ctest_variable}\n"),
                )
            } else if !cmake_value.is_empty() {
                (
                    cmake_value,
                    format!("SetCTestConfiguration from {cmake_variable}:{key}"),
                )
            } else if !env_value.is_empty() {
                (
                    env_value,
                    format!("SetCTestConfiguration from ENV{{{cmake_variable}}}:{key}"),
                )
            } else {
                (String::new(), String::new())
            };
            if !value.is_empty() {
                if first.starts_with("SetCTestConfiguration from") {
                    handler_output(interp, &format!("{first}:{value}\n"), parsed.quiet);
                } else {
                    handler_output(interp, &first, parsed.quiet);
                }
                handler_output(
                    interp,
                    &format!("SetCTestConfiguration:{key}:{value}\n"),
                    parsed.quiet,
                );
                set_step_configuration(interp, key, &value);
            }
        };
        log_configuration("DropMethod", "CTEST_DROP_METHOD", "CMAKE_DROP_METHOD");
        log_configuration("DropSite", "CTEST_DROP_SITE", "CMAKE_DROP_SITE");
        log_configuration("TLSVersion", "CTEST_TLS_VERSION", "CMAKE_TLS_VERSION");
        log_configuration("TLSVerify", "CTEST_TLS_VERIFY", "CMAKE_TLS_VERIFY");
    }
    handler_output(interp, "Submit files\n", parsed.quiet);
    if !specific_group.is_empty() {
        handler_output(
            interp,
            &format!("   Send to group: {specific_group}\n"),
            parsed.quiet,
        );
    }
    let url = submit_url(interp, parsed.submit_url.as_deref());
    handler_output(interp, &format!("   SubmitURL: {url}\n"), parsed.quiet);
    if debug {
        for header in &parsed.headers {
            handler_output(
                interp,
                &format!("   Add HTTP Header: \"{header}\"\n"),
                parsed.quiet,
            );
        }
    }

    let build_name = interp.get_safe("CTEST_BUILD_NAME");
    let build_name = if build_name.is_empty() {
        "(empty)"
    } else {
        &build_name
    };
    let site = interp.get_safe("CTEST_SITE");
    let prefix = format!("{site}___{build_name}___{tag}-{group}___XML___");
    let error_message = submit_error_for(&url);
    let mut failed_file = String::new();
    if let Some(file) = files.first() {
        let local = if Path::new(file).is_file() {
            PathBuf::from(file)
        } else {
            directory.join(file)
        };
        if file == "Done.xml" {
            let done = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Done>\n\t<buildId></buildId>\n\t<time>{}</time>\n</Done>\n",
                epoch_now()
            );
            let _ = std::fs::write(directory.join("Done.xml"), done);
        }
        let remote_name = Path::new(file)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        let _ = std::fmt::Write::write_fmt(
            &mut log,
            format_args!(
                "\tUpload file: {} to {prefix}{remote_name}\n",
                local.display()
            ),
        );
        let verbose_upload = debug
            || interp
                .ext
                .ctest_script
                .as_ref()
                .is_some_and(|state| state.options.extra_verbose);
        if verbose_upload {
            if let Some(version) = interp
                .ext
                .ctest_script
                .as_ref()
                .and_then(|state| state.configuration.get("TLSVersion"))
            {
                if version == "1.1" {
                    handler_output(
                        interp,
                        "  Set CURLOPT_SSLVERSION to CURL_SSLVERSION_TLSv1_1\n",
                        parsed.quiet,
                    );
                }
            }
            if let Some(verify) = interp
                .ext
                .ctest_script
                .as_ref()
                .and_then(|state| state.configuration.get("TLSVerify"))
            {
                handler_output(
                    interp,
                    &format!(
                        "  Set CURLOPT_SSL_VERIFYPEER to {}\n",
                        if crate::util::is_on(verify) {
                            "on"
                        } else {
                            "off"
                        }
                    ),
                    parsed.quiet,
                );
            }
            let size = std::fs::metadata(&local).map_or(0, |meta| meta.len());
            let md5 = md5_file(&local, parsed.internal_test);
            let separator = if url.contains('?') { "&" } else { "?" };
            let upload_url = if Path::new(file).is_file() {
                format!("{url}{separator}FileName={prefix}{remote_name}&MD5={md5}")
            } else {
                let subproject = interp.global_props.get("SubProject").unwrap_or_default();
                format!(
                    "{url}{separator}FileName={prefix}{remote_name}&build={build_name}&site={site}&stamp={tag}-{group}{}{}&MD5={md5}",
                    if subproject.is_empty() {
                        ""
                    } else {
                        "&subproject="
                    },
                    subproject,
                )
            };
            handler_output(
                interp,
                &format!(
                    "Upload file: {} to {upload_url} Size: {size}\n",
                    local.display()
                ),
                parsed.quiet,
            );
        }
        failed_file = local.display().to_string();
    }
    if !failed_file.is_empty() {
        let text = format!(
            "   Error when uploading file: {failed_file}\n   Error message was: {error_message}\n   Problems when submitting via HTTP\n"
        );
        handler_error(interp, &text);
        log.push_str(&text);
        let _ = std::fs::write(temporary.join(format!("LastSubmit_{tag}.log")), log);
        if let Some(variable) = &parsed.return_value {
            interp.set_def(variable, "-1");
        }
        if let Some(variable) = &parsed.build_id {
            interp.set_def(variable, "");
        }
        interp.ext.ctest_script.as_mut().unwrap().had_error = true;
        return Ok(());
    }
    let _ = std::fs::write(temporary.join(format!("LastSubmit_{tag}.log")), log);
    if let Some(variable) = &parsed.return_value {
        interp.set_def(variable, "0");
    }
    Ok(())
}

fn ctest_start(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.is_empty() {
        return err("called with incorrect number of arguments");
    }
    let mut model = None;
    let mut source = None;
    let mut binary = None;
    let mut append = false;
    let mut quiet = false;
    let mut group = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "GROUP" | "TRACK" => {
                index += 1;
                if index >= args.len() || matches!(args[index].as_str(), "APPEND" | "QUIET") {
                    return err(format!("{} argument missing group name", args[index - 1]));
                }
                group = Some(args[index].clone());
            }
            "APPEND" => append = true,
            "QUIET" => quiet = true,
            value if model.is_none() => model = Some(value.to_owned()),
            value if source.is_none() => source = Some(value.to_owned()),
            value if binary.is_none() => binary = Some(value.to_owned()),
            _ => return err("Too many arguments"),
        }
        index += 1;
    }
    let source = source.unwrap_or_else(|| interp.get_safe("CTEST_SOURCE_DIRECTORY"));
    let binary = binary.unwrap_or_else(|| interp.get_safe("CTEST_BINARY_DIRECTORY"));
    if source.is_empty() {
        return err(
            "source directory not specified. Specify source directory as an argument or set CTEST_SOURCE_DIRECTORY",
        );
    }
    if binary.is_empty() {
        return err(
            "binary directory not specified. Specify binary directory as an argument or set CTEST_BINARY_DIRECTORY",
        );
    }
    if model.is_none() && !append {
        return err(
            "no test model specified and APPEND not specified. Specify either a test model or the APPEND argument",
        );
    }
    let source_dir = collapse_input(&source);
    let binary_dir = collapse_input(&binary);
    if interp
        .ext
        .ctest_script
        .as_ref()
        .is_some_and(|state| state.options.extra_verbose)
    {
        handler_output(
            interp,
            &format!(
                "SetCTestConfiguration:SourceDirectory:{source}\nSetCTestConfiguration:BuildDirectory:{binary}\n"
            ),
            false,
        );
    }
    if let Some(model) = &model {
        handler_output(
            interp,
            &format!(
                "Run dashboard with model {model}\n   Source directory: {source}\n   Build directory: {binary}\n"
            ),
            quiet,
        );
    } else {
        handler_output(
            interp,
            &format!(
                "Run dashboard with to-be-determined model\n   Source directory: {source}\n   Build directory: {binary}\n"
            ),
            quiet,
        );
    }
    if let Some(group) = &group {
        handler_output(interp, &format!("   Group: {group}\n"), quiet);
    }

    let temporary = PathBuf::from(&binary_dir).join("Testing/Temporary");
    let _ = std::fs::create_dir_all(&temporary);
    if std::fs::write(temporary.join("LastStart.log"), "").is_err() {
        handler_error(interp, "Cannot create log file: LastStart.log\n");
        interp.ext.ctest_script.as_mut().unwrap().had_error = true;
        return Ok(());
    }
    if !PathBuf::from(&source_dir).is_dir() {
        return err(format!(
            "given source path\n  {source_dir}\nwhich is not an existing directory.  Set CTEST_CHECKOUT_COMMAND to a command line to create it."
        ));
    }

    let config_file = ["CTestConfig.cmake"]
        .into_iter()
        .map(|name| PathBuf::from(&binary_dir).join(name))
        .find(|path| path.is_file())
        .or_else(|| {
            ["CTestConfig.cmake"]
                .into_iter()
                .map(|name| PathBuf::from(&source_dir).join(name))
                .find(|path| path.is_file())
        });
    if let Some(file) = config_file {
        handler_output(
            interp,
            &format!("   Reading ctest configuration file: {}\n", file.display()),
            quiet,
        );
        if !interp.read_list_file(&file.to_string_lossy()) {
            return err(format!("Could not find include file: {}", file.display()));
        }
    }

    let nightly = interp.get_safe("CTEST_NIGHTLY_START_TIME");
    let site = interp.get_safe("CTEST_SITE");
    let build_name = interp.get_safe("CTEST_BUILD_NAME");
    let mut model_name = model.clone().unwrap_or_default();
    let mut group_name = group.clone().unwrap_or_else(|| model_name.clone());
    let mut existing_tag_message = String::new();
    let mut append_failed = false;
    let tag = if append {
        let tag_file = PathBuf::from(&binary_dir).join("Testing/TAG");
        let contents = std::fs::read_to_string(tag_file).unwrap_or_default();
        let mut lines = contents.lines();
        let existing_tag = lines.next().unwrap_or_default().to_owned();
        let existing_group = lines.next().unwrap_or_default().to_owned();
        let existing_model = lines.next().unwrap_or_default().to_owned();
        if existing_tag.is_empty() {
            handler_error(
                interp,
                &format!(
                    "Cannot read existing TAG file in {binary_dir}/Testing
"
                ),
            );
            ctest_command_error(interp, "ctest_start", "unknown error.");
            append_failed = true;
        }
        if !append_failed && model_name.is_empty() {
            if existing_model.is_empty() {
                handler_error(
                    interp,
                    "TAG file does not contain model and no model specified in start command
",
                );
                ctest_command_error(interp, "ctest_start", "unknown error.");
                append_failed = true;
            } else {
                model_name = existing_model.clone();
            }
        } else if !existing_model.is_empty() && existing_model != model_name {
            handler_error(
                interp,
                "Model given in TAG does not match model given in ctest_start()
",
            );
        }
        if group.is_none() {
            group_name = existing_group.clone();
        } else if existing_group != group_name {
            handler_error(
                interp,
                "Group given in TAG does not match group given in ctest_start()
",
            );
        }
        existing_tag_message = format!("  Use existing tag: {existing_tag} - {group_name}\n");
        existing_tag
    } else {
        report::clock("%Y%m%d-%H%M")
    };
    {
        let state = interp.ext.ctest_script.as_mut().unwrap();
        state.source_dir = source_dir;
        state.binary_dir = binary_dir.clone();
        state.model = model_name.clone();
        state.group = group_name.clone();
        state.specific_group = group.clone().unwrap_or_default();
        state.tag = tag.clone();
        state.site = site;
        state.build_name = build_name;
    }
    if interp
        .ext
        .ctest_script
        .as_ref()
        .is_some_and(|state| state.options.extra_verbose)
    {
        let state = interp.ext.ctest_script.as_ref().unwrap();
        handler_output(
            interp,
            &format!(
                "SetCTestConfigurationFromCMakeVariable:Site:CTEST_SITE\nSetCTestConfiguration:Site:{}\nSetCTestConfigurationFromCMakeVariable:BuildName:CTEST_BUILD_NAME\nSetCTestConfiguration:BuildName:{}\n",
                state.site, state.build_name
            ),
            quiet,
        );
    }
    handler_output(
        interp,
        &format!(
            "   Site: {}\n   Build name: {}\n",
            interp.ext.ctest_script.as_ref().unwrap().site,
            if interp
                .ext
                .ctest_script
                .as_ref()
                .unwrap()
                .build_name
                .is_empty()
            {
                "(empty)".into()
            } else {
                interp.ext.ctest_script.as_ref().unwrap().build_name.clone()
            }
        ),
        quiet,
    );
    if append_failed {
        return Ok(());
    }
    if !existing_tag_message.is_empty() {
        handler_output(interp, &existing_tag_message, quiet);
    }
    if model_name.eq_ignore_ascii_case("nightly") && nightly.is_empty() {
        handler_output(
            interp,
            "WARNING: No nightly start time found please set in CTestConfig.cmake or DartConfig.cmake\n",
            quiet,
        );
        interp.ext.ctest_script.as_mut().unwrap().had_error = true;
        return Ok(());
    }
    let testing = PathBuf::from(&binary_dir).join("Testing");
    let _ = std::fs::create_dir_all(&testing);
    if !append {
        let _ = std::fs::write(
            testing.join("TAG"),
            format!("{tag}\n{group_name}\n{model_name}\n"),
        );
    }
    handler_output(interp, &format!("   Use {group_name} tag: {tag}\n"), quiet);
    Ok(())
}

fn test_options(interp: &Interp, binary: &Path) -> Options {
    let state = interp.ext.ctest_script.as_ref().unwrap();
    let mut options = state.options.clone();
    options.script = true;
    options.show = Show::Run;
    options.dir = Some(binary.into());
    if let Some(config) = interp.get_def("CTEST_CONFIGURATION_TYPE") {
        options.config = config.to_owned();
    }
    options.dashboard_tag = state.tag.clone();
    options.dashboard_model = state.model.clone();
    options.dashboard_site = state.site.clone();
    options.dashboard_build_name = state.build_name.clone();
    options.change_id = interp.get_safe("CTEST_CHANGE_ID");
    options.labels_for_subprojects =
        crate::util::expand_list(&interp.get_safe("CTEST_LABELS_FOR_SUBPROJECTS"), false);
    if !options.extra_verbose {
        options.verbose = false;
    }
    if options.timeout <= 0.0 {
        options.timeout = interp
            .get_def("CTEST_TEST_TIMEOUT")
            .map_or(600.0, super::options::number);
    }
    if !options.parallel_explicit {
        options.parallel = match std::env::var("CTEST_PARALLEL_LEVEL") {
            Ok(value) if value.trim().is_empty() => None,
            Ok(value) => value.trim().parse::<usize>().ok().or(Some(1)),
            Err(_) => Some(1),
        };
    }
    options
}

fn ctest_test(interp: &mut Interp, args: &[String]) -> CmdResult {
    let keywords = [
        "BUILD",
        "RETURN_VALUE",
        "START",
        "END",
        "STRIDE",
        "EXCLUDE",
        "INCLUDE",
        "EXCLUDE_LABEL",
        "INCLUDE_LABEL",
        "EXCLUDE_FROM_FILE",
        "INCLUDE_FROM_FILE",
        "EXCLUDE_FIXTURE",
        "EXCLUDE_FIXTURE_SETUP",
        "EXCLUDE_FIXTURE_CLEANUP",
        "PARALLEL_LEVEL",
        "REPEAT",
        "SCHEDULE_RANDOM",
        "SCHEDULE_RANDOM_SEED",
        "STOP_TIME",
        "TEST_LOAD",
        "RESOURCE_SPEC_FILE",
        "OUTPUT_JUNIT",
        "STOP_ON_FAILURE",
        "APPEND",
        "QUIET",
    ];
    let mut values = BTreeMap::<String, String>::new();
    let mut quiet = false;
    let mut stop_on_failure = false;
    let mut index = 0;
    while index < args.len() {
        let key = args[index].as_str();
        match key {
            "QUIET" => quiet = true,
            "APPEND" => {}
            "STOP_ON_FAILURE" => stop_on_failure = true,
            "PARALLEL_LEVEL" => {
                index += 1;
                let value = if index < args.len() && !keywords.contains(&args[index].as_str()) {
                    let value = args[index].clone();
                    index += 1;
                    value
                } else {
                    String::new()
                };
                values.insert(key.into(), value);
                continue;
            }
            _ if keywords.contains(&key) => {
                index += 1;
                if index >= args.len() {
                    return err(format!("{key} argument missing value"));
                }
                values.insert(key.into(), args[index].clone());
            }
            value => return err(format!("called with unknown argument \"{value}\".")),
        }
        index += 1;
    }
    let build = values.get("BUILD").cloned();
    let return_value = values.get("RETURN_VALUE").cloned();
    let fallback = interp.get_safe("CTEST_BINARY_DIRECTORY");
    let state_binary = interp
        .ext
        .ctest_script
        .as_ref()
        .map(|state| state.binary_dir.clone())
        .unwrap_or_default();
    let binary = build
        .or_else(|| (!fallback.is_empty()).then_some(fallback))
        .or_else(|| (!state_binary.is_empty()).then_some(state_binary));
    let Some(binary) = binary else {
        handler_error(interp, "CTEST_BINARY_DIRECTORY not set\n");
        interp.ext.ctest_script.as_mut().unwrap().had_error = true;
        return Ok(());
    };
    let binary = PathBuf::from(collapse_input(&binary));
    let handler_verbose = interp
        .ext
        .ctest_script
        .as_ref()
        .is_some_and(|state| state.options.verbose);
    if interp
        .ext
        .ctest_script
        .as_ref()
        .is_some_and(|state| state.options.extra_verbose)
    {
        let state = interp.ext.ctest_script.as_ref().unwrap();
        handler_output(
            interp,
            &format!(
                "SetCTestConfiguration:BuildDirectory:{}\nSetCTestConfiguration:SourceDirectory:{}\n",
                binary.display(),
                if state.source_dir.is_empty() {
                    binary.display().to_string()
                } else {
                    state.source_dir.clone()
                }
            ),
            false,
        );
    }
    let mut options = test_options(interp, &binary);
    if let Some(value) = values.get("INCLUDE") {
        options.include = value.clone();
    }
    if let Some(value) = values.get("EXCLUDE") {
        options.exclude = value.clone();
    }
    if let Some(value) = values.get("INCLUDE_LABEL") {
        options.include_labels.push(value.clone());
    }
    if let Some(value) = values.get("EXCLUDE_LABEL") {
        options.exclude_labels.push(value.clone());
    }
    if let Some(value) = values.get("INCLUDE_FROM_FILE") {
        options.name_file = Some(value.into());
    }
    if let Some(value) = values.get("EXCLUDE_FROM_FILE") {
        options.exclude_name_file = Some(value.into());
    }
    if let Some(value) = values.get("EXCLUDE_FIXTURE") {
        options.exclude_fixture = value.clone();
    }
    if let Some(value) = values.get("EXCLUDE_FIXTURE_SETUP") {
        options.exclude_setup = value.clone();
    }
    if let Some(value) = values.get("EXCLUDE_FIXTURE_CLEANUP") {
        options.exclude_cleanup = value.clone();
    }
    let start = values.get("START").map(String::as_str).unwrap_or_default();
    let end = values.get("END").map(String::as_str).unwrap_or_default();
    let stride = values.get("STRIDE").map(String::as_str).unwrap_or_default();
    if !start.is_empty() || !end.is_empty() || !stride.is_empty() {
        options.index = format!("{start},{end},{stride}");
    }
    if let Some(value) = values.get("PARALLEL_LEVEL") {
        if value.is_empty() {
            options.parallel = None;
        } else {
            match value.parse::<usize>() {
                Ok(level) => options.parallel = Some(level),
                Err(_) => {
                    handler_error(interp, &format!("ParallelLevel invalid value: {value}\n"));
                    interp.ext.ctest_script.as_mut().unwrap().had_error = true;
                    return Ok(());
                }
            }
        }
    }
    if let Some(value) = values.get("REPEAT") {
        if let Some((mode, count)) = value.split_once(':')
            && let Ok(count) = count.parse::<usize>()
        {
            let mode = match mode {
                "UNTIL_FAIL" => Some(Repeat::UntilFail),
                "UNTIL_PASS" => Some(Repeat::UntilPass),
                "AFTER_TIMEOUT" => Some(Repeat::AfterTimeout),
                _ => None,
            };
            if let Some(mode) = mode {
                options.repeat = Some((mode, count));
            }
        }
        if options.repeat.is_none() {
            handler_error(interp, &format!("Repeat option invalid value: {value}\n"));
            interp.ext.ctest_script.as_mut().unwrap().had_error = true;
            if let Some(variable) = &return_value {
                interp.set_def(variable, "-1");
            }
            return Ok(());
        }
    }
    if let Some(value) = values.get("SCHEDULE_RANDOM") {
        options.random = crate::util::is_on(value);
    }
    if let Some(value) = values.get("SCHEDULE_RANDOM_SEED")
        && let Ok(seed) = value.parse::<u32>()
    {
        options.seed = Some(seed);
    }
    if let Some(value) = values.get("TEST_LOAD") {
        match value.parse::<u64>() {
            Ok(load) => options.test_load = Some(load),
            Err(_) => {
                options.test_load = Some(0);
                handler_error(
                    interp,
                    &format!("Invalid value for 'TEST_LOAD' : {value}\n"),
                );
            }
        }
    } else if let Some(value) = interp
        .get_def("CTEST_TEST_LOAD")
        .filter(|value| !value.is_empty())
    {
        match value.parse::<u64>() {
            Ok(load) => options.test_load = Some(load),
            Err(_) => {
                options.test_load = Some(0);
                handler_error(
                    interp,
                    &format!("Invalid value for 'CTEST_TEST_LOAD' : {value}\n"),
                );
            }
        }
    }
    if let Some(value) = values.get("RESOURCE_SPEC_FILE") {
        options.resource_spec = Some(value.into());
    } else if let Some(value) = interp
        .get_def("CTEST_RESOURCE_SPEC_FILE")
        .filter(|value| !value.is_empty())
    {
        options.resource_spec = Some(value.into());
    }
    if let Some(value) = values.get("OUTPUT_JUNIT") {
        options.junit = Some(value.into());
    }
    options.stop_on_failure = stop_on_failure;
    if let Some(value) = interp.get_def("CTEST_CUSTOM_MAXIMUM_PASSED_TEST_OUTPUT_SIZE")
        && let Ok(size) = value.parse::<usize>()
    {
        options.output_size_passed = Some(size);
    }
    if let Some(value) = interp.get_def("CTEST_CUSTOM_MAXIMUM_FAILED_TEST_OUTPUT_SIZE")
        && let Ok(size) = value.parse::<usize>()
    {
        options.output_size_failed = Some(size);
    }
    if let Some(value) = interp.get_def("CTEST_CUSTOM_TEST_OUTPUT_TRUNCATION") {
        options.output_truncation = value.to_owned();
    }
    options.script_quiet = quiet;
    let mut log = Log::new(&options);
    log.suppress_output = !handler_verbose;
    if !binary.is_dir() {
        let error = super::directory_error(&binary);
        handler_error(interp, &format!("{error}\n"));
        interp.ext.ctest_script.as_mut().unwrap().had_error = true;
        return Ok(());
    }
    let old_dir = std::env::current_dir().unwrap_or_default();
    let (code, no_tests) = execute_detailed(binary, options, &mut log);
    let system_error = log.system_error;
    let _ = std::env::set_current_dir(old_dir);
    // Dashboard handlers use legacy no-tests semantics unless the explicit
    // error action is selected: an empty test set is not itself a handler
    // failure, even though normal ctest exits with TEST_ERRORS.
    let result_code = if no_tests && code == 8 { 0 } else { code };
    if let Some(variable) = return_value {
        interp.set_def(variable, if result_code == 0 { "0" } else { "-1" });
    }
    if no_tests || system_error {
        interp.ext.ctest_script.as_mut().unwrap().had_error = true;
    }
    Ok(())
}

fn ctest_sleep(_interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() == 1 {
        std::thread::sleep(std::time::Duration::from_secs(
            integer(&args[0]).max(0) as u64
        ));
        return Ok(());
    }
    if args.len() == 3 {
        let time1 = integer(&args[0]).max(0) as u64;
        let duration = integer(&args[1]).max(0) as u64;
        let time2 = integer(&args[2]).max(0) as u64;
        if time1 + duration > time2 {
            std::thread::sleep(std::time::Duration::from_secs(time1 + duration - time2));
        }
        return Ok(());
    }
    err("called with incorrect number of arguments")
}

fn ctest_empty_binary_directory(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() != 1 {
        return err("called with incorrect number of arguments");
    }
    let path = PathBuf::from(&args[0]);
    let reason = if args[0].len() < 2 {
        Some("path too short".to_owned())
    } else if !path.exists() {
        None
    } else if !path.join("CMakeCache.txt").exists() {
        Some("path does not contain an existing CMakeCache.txt file".to_owned())
    } else {
        let mut result = Ok(());
        for attempt in 0..5 {
            result = std::fs::remove_dir_all(&path);
            if result.is_ok() {
                break;
            }
            if attempt + 1 < 5 {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
        result.err().map(|error| error.to_string())
    };
    if let Some(reason) = reason {
        interp.issue_at_unsuppressed_no_top(
            crate::messages::MessageType::FatalError,
            &format!(
                "Did not remove the binary directory:\n {}\nbecause:\n {reason}",
                args[0]
            ),
        );
    }
    Ok(())
}

fn read_custom_tree(interp: &mut Interp, dir: &Path) {
    for name in ["CTestCustom.cmake", "CTestCustom.ctest"] {
        let file = dir.join(name);
        if file.is_file() {
            if !interp.read_list_file(&file.to_string_lossy()) || interp.error {
                handler_error(
                    interp,
                    &format!("Problem reading custom configuration: {}\n", file.display()),
                );
            }
            return;
        }
    }
}

fn ctest_read_custom_files(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.is_empty() {
        return err("called with incorrect number of arguments");
    }
    for arg in args {
        read_custom_tree(interp, Path::new(arg));
    }
    Ok(())
}

fn ctest_run_script(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.is_empty() {
        return err("called with incorrect number of arguments");
    }
    let mut start = 0;
    if args.first().is_some_and(|arg| arg == "NEW_PROCESS") {
        start = 1;
    }
    let mut return_variable = String::new();
    for index in start..args.len() {
        if args[index] == "RETURN_VALUE" && index + 1 < args.len() {
            return_variable = args[index + 1].clone();
        }
    }
    let options = interp
        .ext
        .ctest_script
        .as_ref()
        .map(|state| state.options.clone())
        .unwrap_or_default();
    for index in start..args.len() {
        if args[index] == "RETURN_VALUE" {
            continue;
        }
        if index > start && args[index - 1] == "RETURN_VALUE" {
            continue;
        }
        let script = collapse_input(&args[index]);
        let result = run_one(&script, true, &options);
        interp.set_def(&return_variable, result.to_string());
    }
    Ok(())
}

fn epoch_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

fn split_options(value: &str) -> Vec<String> {
    crate::util::expand_list(value, false)
}

fn shell_command(command: &str, directory: &Path) -> (String, i32) {
    let result = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("{command} 2>&1"))
        .current_dir(directory)
        .stdin(std::process::Stdio::null())
        .output();
    match result {
        Ok(result) => (
            crate::bytes::decode(&result.stdout),
            result.status.code().unwrap_or(-1),
        ),
        Err(error) => (format!("{error}\n"), -1),
    }
}

fn dashboard_options(interp: &Interp, binary: &Path) -> Options {
    let state = interp.ext.ctest_script.as_ref().unwrap();
    let mut options = state.options.clone();
    options.dashboard_tag = state.tag.clone();
    options.dashboard_model = state.model.clone();
    options.dashboard_site = state.site.clone();
    options.dashboard_build_name = state.build_name.clone();
    options.dir = Some(binary.into());
    options.labels_for_subprojects =
        crate::util::expand_list(&interp.get_safe("CTEST_LABELS_FOR_SUBPROJECTS"), false);
    options
}

fn step_log_name(step: &str, tag: &str) -> String {
    if tag.is_empty() {
        format!("Last{step}.log")
    } else {
        format!("Last{step}_{tag}.log")
    }
}

fn parse_handler_args(
    args: &[String],
    values: &[&str],
    flags: &[&str],
) -> Result<
    (
        std::collections::BTreeMap<String, String>,
        std::collections::BTreeSet<String>,
    ),
    String,
> {
    let mut map = std::collections::BTreeMap::new();
    let mut present = std::collections::BTreeSet::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if values.contains(&arg.as_str()) {
            if index + 1 >= args.len() {
                return Err(format!("{arg} argument missing value"));
            }
            map.insert(arg.clone(), args[index + 1].clone());
            index += 2;
        } else if flags.contains(&arg.as_str()) {
            present.insert(arg.clone());
            index += 1;
        } else if !map.contains_key("BUILD") && values.contains(&"BUILD") {
            map.insert("BUILD".into(), arg.clone());
            index += 1;
        } else {
            return Err(format!("called with unknown argument \"{arg}\"."));
        }
    }
    Ok((map, present))
}

fn required_build_directory(
    interp: &Interp,
    parsed: &std::collections::BTreeMap<String, String>,
) -> Result<String, String> {
    let build = parsed
        .get("BUILD")
        .cloned()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| interp.get_safe("CTEST_BINARY_DIRECTORY"));
    if build.is_empty() {
        Err("called with no build directory specified.  Either use the BUILD argument or set the CTEST_BINARY_DIRECTORY variable.".into())
    } else {
        Ok(build)
    }
}

fn ctest_configure(interp: &mut Interp, args: &[String]) -> CmdResult {
    let (parsed, present) = match parse_handler_args(
        args,
        &["BUILD", "SOURCE", "OPTIONS", "RETURN_VALUE", "SUBMIT_INDEX"],
        &["APPEND", "QUIET"],
    ) {
        Ok(parsed) => parsed,
        Err(error) => return err(error),
    };
    let build = match required_build_directory(interp, &parsed) {
        Ok(build) => build,
        Err(error) => return err(error),
    };
    let quiet = present.contains("QUIET");
    let mut command = interp.get_safe("CTEST_CONFIGURE_COMMAND");
    if command.is_empty() {
        let generator = interp.get_safe("CTEST_CMAKE_GENERATOR");
        if generator.is_empty() {
            return err(
                "called with no configure command specified.  If this is a  \"built with CMake\" project, set CTEST_CMAKE_GENERATOR. If not, set CTEST_CONFIGURE_COMMAND.",
            );
        }
        let source = parsed
            .get("SOURCE")
            .cloned()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| interp.get_safe("CTEST_SOURCE_DIRECTORY"));
        if source.is_empty() || !Path::new(&source).join("CMakeLists.txt").is_file() {
            return err(
                "called with invalid source directory.  CTEST_SOURCE_DIRECTORY must be set to a directory that contains CMakeLists.txt.",
            );
        }
        let source = collapse_input(&source);
        let build_collapsed = collapse_input(&build);
        let cmake = interp.get_safe("CMAKE_EXECUTABLE_NAME");
        command = format!("\"{cmake}\"");
        if let Some(options) = parsed.get("OPTIONS") {
            for option in split_options(options) {
                let _ = write!(command, " \"{option}\"");
            }
        }
        let multi = generator.contains("Multi-Config")
            || generator.contains("Visual Studio")
            || generator == "Xcode";
        let options_text = parsed.get("OPTIONS").map_or("", String::as_str);
        let build_type = interp.get_safe("CTEST_CONFIGURATION_TYPE");
        if !multi
            && !build_type.is_empty()
            && !options_text.contains("CMAKE_BUILD_TYPE=")
            && !options_text.contains("CMAKE_BUILD_TYPE:STRING=")
        {
            let _ = write!(command, " \"-DCMAKE_BUILD_TYPE:STRING={build_type}\"");
        }
        if interp.is_on("CTEST_USE_LAUNCHERS") {
            command.push_str(" \"-DCTEST_USE_LAUNCHERS:BOOL=TRUE\"");
        }
        let _ = write!(command, " \"-G{generator}\"");
        let platform = interp.get_safe("CTEST_CMAKE_GENERATOR_PLATFORM");
        if !platform.is_empty() {
            let _ = write!(command, " \"-A{platform}\"");
        }
        let toolset = interp.get_safe("CTEST_CMAKE_GENERATOR_TOOLSET");
        if !toolset.is_empty() {
            let _ = write!(command, " \"-T{toolset}\"");
        }
        let _ = write!(command, " \"-S{source}\" \"-B{build_collapsed}\"");
    }
    handler_output(interp, "Configure project\n", quiet);
    let build_path = PathBuf::from(collapse_input(&build));
    if let Err(error) = std::fs::create_dir_all(&build_path) {
        return err(format!(
            "cannot create directory {}: {error}",
            build_path.display()
        ));
    }
    if interp
        .ext
        .ctest_script
        .as_ref()
        .is_some_and(|state| state.options.extra_verbose)
    {
        handler_output(
            interp,
            &format!("Configure with command: {command}\n"),
            quiet,
        );
    }
    let start = epoch_now();
    let (output, status) = shell_command(&command, &build_path);
    let end = epoch_now();
    let state = interp.ext.ctest_script.as_ref().unwrap();
    let temporary = build_path.join("Testing/Temporary");
    let _ = std::fs::create_dir_all(&temporary);
    let _ = std::fs::write(
        temporary.join(step_log_name("Configure", &state.tag)),
        crate::bytes::encode(&output),
    );
    let kib = output.len() / 1024;
    let dots = output.len().div_ceil(1024).max(1);
    handler_output(
        interp,
        &format!(
            "   Each . represents 1024 bytes of output\n    {} Size of output: {kib}K\n",
            ".".repeat(dots)
        ),
        quiet,
    );
    if status != 0 {
        handler_error(interp, "Error(s) when configuring the project\n");
    }
    if let Some(variable) = parsed.get("RETURN_VALUE") {
        interp.set_def(variable, status.to_string());
    }
    let options = dashboard_options(interp, &build_path);
    let _ = report::configure_xml(&build_path, &options, &command, &output, status, start, end);
    if status != 0 {
        interp.ext.ctest_script.as_mut().unwrap().had_error = true;
    }
    Ok(())
}

fn ctest_build(interp: &mut Interp, args: &[String]) -> CmdResult {
    let (parsed, present) = match parse_handler_args(
        args,
        &[
            "BUILD",
            "RETURN_VALUE",
            "NUMBER_ERRORS",
            "NUMBER_WARNINGS",
            "TARGET",
            "CONFIGURATION",
            "FLAGS",
            "PROJECT_NAME",
            "PARALLEL_LEVEL",
        ],
        &["APPEND", "QUIET"],
    ) {
        Ok(parsed) => parsed,
        Err(error) => return err(error),
    };
    let build = match required_build_directory(interp, &parsed) {
        Ok(build) => build,
        Err(error) => return err(error),
    };
    let quiet = present.contains("QUIET");
    let mut command = interp.get_safe("CTEST_BUILD_COMMAND");
    if command.is_empty() {
        let generator = interp.get_safe("CTEST_CMAKE_GENERATOR");
        if generator.is_empty() {
            return err(
                "has no project to build. If this is a \"built with CMake\" project, verify that CTEST_CMAKE_GENERATOR is set. Otherwise, set CTEST_BUILD_COMMAND to build the project with a custom command line.",
            );
        }
        let configuration = parsed
            .get("CONFIGURATION")
            .cloned()
            .filter(|value| !value.is_empty())
            .or_else(|| {
                let value = interp.get_safe("CTEST_BUILD_CONFIGURATION");
                (!value.is_empty()).then_some(value)
            })
            .or_else(|| {
                let value = interp.get_safe("CTEST_CONFIGURATION_TYPE");
                (!value.is_empty()).then_some(value)
            })
            .unwrap_or_else(|| "Release".into());
        let target = parsed
            .get("TARGET")
            .cloned()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| interp.get_safe("CTEST_BUILD_TARGET"));
        let cmake = interp.get_safe("CMAKE_EXECUTABLE_NAME");
        command = format!("{cmake} --build . --config \"{configuration}\"");
        if !target.is_empty() {
            let _ = write!(command, " --target \"{target}\"");
        }
        if let Some(parallel) = parsed
            .get("PARALLEL_LEVEL")
            .filter(|value| !value.is_empty())
        {
            let _ = write!(command, " --parallel {parallel}");
        }
        let flags = parsed
            .get("FLAGS")
            .cloned()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| interp.get_safe("CTEST_BUILD_FLAGS"));
        if !flags.is_empty() {
            let _ = write!(command, " -- {flags}");
        }
    }
    handler_output(interp, "Build project\n", quiet);
    handler_output(
        interp,
        "   Each symbol represents 1024 bytes of output.\n   '!' represents an error and '*' a warning.\n",
        quiet,
    );
    let build_path = PathBuf::from(collapse_input(&build));
    let start = epoch_now();
    let (output, status) = shell_command(&command, &build_path);
    let end = epoch_now();
    let lines: Vec<&str> = output.lines().collect();
    let mut diagnostics = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let lower = line.to_ascii_lowercase();
        let is_error = lower.contains("error") && !lower.contains("warning");
        let is_warning = lower.contains("warning");
        if is_error || is_warning {
            diagnostics.push(report::BuildDiagnostic {
                error: is_error,
                line: index + 1,
                text: (*line).to_owned(),
                pre_context: lines[index.saturating_sub(2)..index].join("\n"),
                post_context: lines[index + 1..(index + 3).min(lines.len())].join("\n"),
            });
        }
    }
    let errors = diagnostics.iter().filter(|item| item.error).count();
    let warnings = diagnostics.iter().filter(|item| !item.error).count();
    let state = interp.ext.ctest_script.as_ref().unwrap();
    let temporary = build_path.join("Testing/Temporary");
    let _ = std::fs::create_dir_all(&temporary);
    let _ = std::fs::write(
        temporary.join(step_log_name("Build", &state.tag)),
        crate::bytes::encode(&output),
    );
    let kib = output.len() / 1024;
    let dots = output.len().div_ceil(1024).max(1);
    handler_output(
        interp,
        &format!("    {} Size of output: {kib}K\n", ".".repeat(dots)),
        quiet,
    );
    let options = dashboard_options(interp, &build_path);
    let _ = report::build_xml(&build_path, &options, &command, &diagnostics, start, end);
    if status != 0 || errors != 0 {
        handler_error(interp, "Error(s) when building project\n");
        interp.ext.ctest_script.as_mut().unwrap().had_error = true;
    }
    handler_output(interp, &format!("   {errors} Compiler errors\n"), quiet);
    handler_output(interp, &format!("   {warnings} Compiler warnings\n"), quiet);
    if let Some(variable) = parsed.get("RETURN_VALUE") {
        interp.set_def(variable, status.to_string());
    }
    if let Some(variable) = parsed.get("NUMBER_ERRORS") {
        interp.set_def(variable, errors.to_string());
    }
    if let Some(variable) = parsed.get("NUMBER_WARNINGS") {
        interp.set_def(variable, warnings.to_string());
    }
    Ok(())
}

fn register(interp: &mut Interp) {
    interp.register("ctest_start", ctest_start);
    interp.register("ctest_test", ctest_test);
    interp.register("ctest_sleep", ctest_sleep);
    interp.register("ctest_empty_binary_directory", ctest_empty_binary_directory);
    interp.register("ctest_read_custom_files", ctest_read_custom_files);
    interp.register("ctest_run_script", ctest_run_script);
    interp.register("ctest_build", ctest_build);
    interp.register("ctest_configure", ctest_configure);
    interp.register("ctest_coverage", ctest_coverage);
    interp.register("ctest_memcheck", ctest_memcheck);
    interp.register("ctest_submit", ctest_submit);
    interp.register("ctest_update", ctest_update);
    interp.register("ctest_upload", ctest_upload);
}

fn run_one(total_script: &str, _in_process: bool, options: &Options) -> i32 {
    let (script, script_arg) = total_script
        .split_once(',')
        .map_or((total_script, ""), |(script, arg)| (script, arg));
    let script = &collapse_input(script);
    if !Path::new(script).is_file() {
        eprintln!("CMake Error: Cannot find file: {script}");
        return 1;
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    let mut interp = testfile::interpreter(&cwd);
    register(&mut interp);
    interp.ext.ctest_script = Some(State::new(options));
    let path = PathBuf::from(script);
    interp.set_def(
        "CTEST_SCRIPT_DIRECTORY",
        path.parent()
            .unwrap_or_else(|| Path::new(""))
            .to_string_lossy(),
    );
    interp.set_def(
        "CTEST_SCRIPT_NAME",
        path.file_name().unwrap_or_default().to_string_lossy(),
    );
    interp.set_def(
        "CTEST_EXECUTABLE_NAME",
        super::sibling("ctest").to_string_lossy(),
    );
    interp.set_def(
        "CMAKE_EXECUTABLE_NAME",
        super::sibling("cmake").to_string_lossy(),
    );
    interp.set_def("CTEST_ELAPSED_TIME", "0");
    if !options.config.is_empty() {
        interp.set_def("CTEST_CONFIGURATION_TYPE", &options.config);
    }
    if !script_arg.is_empty() {
        interp.set_def("CTEST_SCRIPT_ARG", script_arg);
    }
    for (name, value) in &options.definitions {
        interp.set_def(name, value);
    }
    if options.extra_verbose {
        let absolute = collapse_input(script);
        handler_output(
            &interp,
            &format!("* Extra verbosity turned on\nReading Script: {absolute}\n"),
            false,
        );
    }
    let ok = interp.read_list_file(script);
    let had_error = interp
        .ext
        .ctest_script
        .as_ref()
        .is_some_and(|state| state.had_error);
    if let Some(code) = interp.exit_code {
        return code;
    }
    if !ok || interp.error || interp.fatal || had_error {
        -1
    } else {
        0
    }
}

pub(super) fn run_scripts(options: &Options) -> i32 {
    if let Some(path) = &options.output_log {
        let _ = std::fs::File::create(path);
    }
    let mut result = 0;
    for (script, in_process) in &options.scripts {
        result |= run_one(&script.to_string_lossy(), *in_process, options);
    }
    result
}

fn step_error(command: &str, error: crate::interp::CmdErr) {
    if let crate::interp::CmdErr::Error(message) = error {
        let frame = crate::messages::Frame {
            file: ".".into(),
            line: 0,
            name: None,
            deferred: false,
        };
        eprintln!(
            "{}",
            crate::messages::render(
                crate::messages::MessageType::FatalError,
                &format!("{command} {message}"),
                &[frame],
                None,
            )
        );
    }
}

fn set_step_configuration(interp: &mut Interp, key: &str, value: &str) {
    if value.is_empty() {
        return;
    }
    if interp
        .ext
        .ctest_script
        .as_ref()
        .is_some_and(|state| state.options.extra_verbose)
    {
        handler_output(
            interp,
            &format!("SetCTestConfiguration:{key}:{value}\n"),
            false,
        );
    }
    interp
        .ext
        .ctest_script
        .as_mut()
        .unwrap()
        .configuration
        .insert(key.into(), value.into());
}

fn set_cmake_variables(interp: &mut Interp) {
    let configuration = interp
        .ext
        .ctest_script
        .as_ref()
        .unwrap()
        .configuration
        .clone();
    for (cmake_var, config_key) in [
        ("CTEST_SITE", "Site"),
        ("CTEST_BUILD_NAME", "BuildName"),
        ("CTEST_NIGHTLY_START_TIME", "NightlyStartTime"),
        ("CTEST_SOURCE_DIRECTORY", "SourceDirectory"),
        ("CTEST_BINARY_DIRECTORY", "BuildDirectory"),
        ("CTEST_UPDATE_COMMAND", "UpdateCommand"),
        ("CTEST_UPDATE_OPTIONS", "UpdateOptions"),
        ("CTEST_UPDATE_TYPE", "UpdateType"),
        ("CTEST_CVS_COMMAND", "CVSCommand"),
        ("CTEST_CVS_UPDATE_OPTIONS", "CVSUpdateOptions"),
        ("CTEST_SVN_COMMAND", "SVNCommand"),
        ("CTEST_SVN_UPDATE_OPTIONS", "SVNUpdateOptions"),
        ("CTEST_SVN_OPTIONS", "SVNOptions"),
        ("CTEST_BZR_COMMAND", "BZRCommand"),
        ("CTEST_BZR_UPDATE_OPTIONS", "BZRUpdateOptions"),
        ("CTEST_GIT_COMMAND", "GITCommand"),
        ("CTEST_GIT_UPDATE_OPTIONS", "GITUpdateOptions"),
        ("CTEST_GIT_INIT_SUBMODULES", "GITInitSubmodules"),
        ("CTEST_GIT_UPDATE_CUSTOM", "GITUpdateCustom"),
        ("CTEST_UPDATE_VERSION_ONLY", "UpdateVersionOnly"),
        ("CTEST_UPDATE_VERSION_OVERRIDE", "UpdateVersionOverride"),
        ("CTEST_HG_COMMAND", "HGCommand"),
        ("CTEST_HG_UPDATE_OPTIONS", "HGUpdateOptions"),
        ("CTEST_P4_COMMAND", "P4Command"),
        ("CTEST_P4_UPDATE_CUSTOM", "P4UpdateCustom"),
        ("CTEST_P4_UPDATE_OPTIONS", "P4UpdateOptions"),
        ("CTEST_P4_CLIENT", "P4Client"),
        ("CTEST_P4_OPTIONS", "P4Options"),
        ("CTEST_CONFIGURE_COMMAND", "ConfigureCommand"),
        ("CTEST_LABELS_FOR_SUBPROJECTS", "LabelsForSubprojects"),
        ("CTEST_BUILD_COMMAND", "MakeCommand"),
        ("CTEST_USE_LAUNCHERS", "UseLaunchers"),
        ("CTEST_COVERAGE_COMMAND", "CoverageCommand"),
        ("CTEST_COVERAGE_EXTRA_FLAGS", "CoverageExtraFlags"),
        ("CTEST_MEMORYCHECK_TYPE", "MemoryCheckType"),
        (
            "CTEST_MEMORYCHECK_SANITIZER_OPTIONS",
            "MemoryCheckSanitizerOptions",
        ),
        ("CTEST_MEMORYCHECK_COMMAND", "MemoryCheckCommand"),
        (
            "CTEST_MEMORYCHECK_COMMAND_OPTIONS",
            "MemoryCheckCommandOptions",
        ),
        (
            "CTEST_MEMORYCHECK_SUPPRESSIONS_FILE",
            "MemoryCheckSuppressionFile",
        ),
        ("CTEST_SUBMIT_URL", "SubmitURL"),
        ("CTEST_DROP_METHOD", "DropMethod"),
        ("CTEST_DROP_SITE_USER", "DropSiteUser"),
        ("CTEST_DROP_SITE_PASSWORD", "DropSitePassword"),
    ] {
        if let Some(value) = configuration
            .get(config_key)
            .filter(|value| !value.is_empty())
        {
            if interp
                .ext
                .ctest_script
                .as_ref()
                .is_some_and(|state| state.options.extra_verbose)
            {
                handler_output(
                    interp,
                    &format!("SetCMakeVariable:{cmake_var}:{value}\n"),
                    false,
                );
            }
            interp.set_def(cmake_var, value);
        }
    }
}

fn update_dashboard_configuration(interp: &mut Interp, binary: &Path) {
    let ini = binary.join("CTestConfiguration.ini");
    let tcl = binary.join("DartConfiguration.tcl");
    let file = if ini.is_file() { ini } else { tcl };
    if interp
        .ext
        .ctest_script
        .as_ref()
        .is_some_and(|state| state.options.extra_verbose)
    {
        handler_output(
            interp,
            &format!("UpdateCTestConfiguration  from :{}\n", file.display()),
            false,
        );
    }
    if !file.is_file() {
        eprintln!("Cannot find file: {}", file.display());
    } else if let Ok(contents) = std::fs::read_to_string(&file) {
        if interp
            .ext
            .ctest_script
            .as_ref()
            .is_some_and(|state| state.options.extra_verbose)
        {
            handler_output(
                interp,
                &format!("Parse Config file:{}\n", file.display()),
                false,
            );
        }
        let mut lines = contents.lines();
        while let Some(line) = lines.next() {
            let mut line = line.trim().to_owned();
            while line.ends_with('\\') {
                line.pop();
                let Some(next) = lines.next() else { break };
                line.push_str(next.trim());
            }
            if !line.is_empty()
                && !line.starts_with('#')
                && let Some((key, value)) = line.split_once(':')
            {
                set_step_configuration(interp, key, value.trim());
            }
        }
    }
    set_cmake_variables(interp);
}

fn read_step_tag(interp: &mut Interp, binary: &Path) -> bool {
    let Ok(contents) = std::fs::read_to_string(binary.join("Testing/TAG")) else {
        return false;
    };
    let mut lines = contents.lines();
    let (Some(tag), Some(group), Some(model)) = (lines.next(), lines.next(), lines.next()) else {
        return false;
    };
    let state = interp.ext.ctest_script.as_mut().unwrap();
    state.tag = tag.into();
    state.group = group.into();
    state.model = model.into();
    state.binary_dir = binary.to_string_lossy().into_owned();
    true
}

fn create_step_tag(interp: &mut Interp, binary: &Path, model: &str, group: &str) -> bool {
    let tag = report::clock("%Y%m%d-%H%M");
    let testing = binary.join("Testing");
    if std::fs::create_dir_all(&testing).is_err() {
        return false;
    }
    if std::fs::write(testing.join("TAG"), format!("{tag}\n{group}\n{model}\n")).is_err() {
        return false;
    }
    let state = interp.ext.ctest_script.as_mut().unwrap();
    state.tag = tag;
    state.group = group.into();
    state.model = model.into();
    state.binary_dir = binary.to_string_lossy().into_owned();
    true
}

fn invoke_dashboard_command(
    interp: &mut Interp,
    command: fn(&mut Interp, &[String]) -> CmdResult,
    name: &str,
) -> bool {
    let args = ["RETURN_VALUE".to_owned(), "return_value".to_owned()];
    match command(interp, &args) {
        Ok(()) => interp.get_safe("return_value").parse::<i32>().unwrap_or(0) >= 0,
        Err(error) => {
            step_error(name, error);
            false
        }
    }
}

pub(super) fn process_steps(options: &Options) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_default();
    let binary = options.dir.as_ref().map_or_else(
        || cwd.clone(),
        |dir| {
            PathBuf::from(collapse_full_path(
                &dir.to_string_lossy(),
                &cwd.to_string_lossy(),
            ))
        },
    );
    if let Err(error) = std::env::set_current_dir(&binary) {
        eprintln!(
            "Failed to change working directory to \"{}\": {error}",
            binary.display()
        );
        return 1;
    }

    let mut interp = testfile::interpreter(&binary);
    register(&mut interp);
    let mut step_options = options.clone();
    step_options.extra_verbose = options.verbose;
    step_options.verbose = true;
    interp.ext.ctest_script = Some(State::new(&step_options));
    {
        let state = interp.ext.ctest_script.as_mut().unwrap();
        state.process_steps = true;
        state.binary_dir = binary.to_string_lossy().into_owned();
        state.site.clear();
        state.build_name.clear();
        state.model = options.model.clone();
        state.group = if options.group.is_empty() {
            options.model.clone()
        } else {
            options.group.clone()
        };
        state.specific_group = options.group.clone();
    }

    let binary_text = binary.to_string_lossy().into_owned();
    set_step_configuration(&mut interp, "BuildDirectory", &binary_text);
    update_dashboard_configuration(&mut interp, &binary);

    let parts = &options.dashboard_parts;
    if parts.contains("start") {
        let model = if options.model.is_empty() {
            "Experimental"
        } else {
            options.model.as_str()
        };
        let group = if options.group.is_empty() {
            model
        } else {
            &options.group
        };
        let args = [model.to_owned(), "GROUP".to_owned(), group.to_owned()];
        if let Err(error) = ctest_start(&mut interp, &args) {
            step_error("ctest_start", error);
            return 12;
        }
    } else if !read_step_tag(&mut interp, &binary)
        && !create_step_tag(
            &mut interp,
            &binary,
            if options.model.is_empty() {
                "Experimental"
            } else {
                &options.model
            },
            if options.group.is_empty() {
                if options.model.is_empty() {
                    "Experimental"
                } else {
                    &options.model
                }
            } else {
                &options.group
            },
        )
    {
        eprintln!("Problem initializing the dashboard.");
        return 12;
    }

    let mut result = 0;
    if parts.contains("update")
        && !invoke_dashboard_command(&mut interp, ctest_update, "ctest_update")
    {
        result |= 0x01;
    }
    if options.model.eq_ignore_ascii_case("continuous")
        && !crate::util::is_on(&interp.get_safe("return_value"))
    {
        return 0;
    }
    if parts.contains("configure")
        && !invoke_dashboard_command(&mut interp, ctest_configure, "ctest_configure")
    {
        result |= 0x02;
    }
    if parts.contains("build") {
        update_dashboard_configuration(&mut interp, &binary);
        if !invoke_dashboard_command(&mut interp, ctest_build, "ctest_build") {
            result |= 0x04;
        }
    }
    if parts.contains("test") {
        update_dashboard_configuration(&mut interp, &binary);
        if !invoke_dashboard_command(&mut interp, ctest_test, "ctest_test") {
            result |= 0x08;
        }
    }
    if parts.contains("coverage") {
        update_dashboard_configuration(&mut interp, &binary);
        if !invoke_dashboard_command(&mut interp, ctest_coverage, "ctest_coverage") {
            result |= 0x20;
        }
    }
    if parts.contains("memcheck") {
        update_dashboard_configuration(&mut interp, &binary);
        if !invoke_dashboard_command(&mut interp, ctest_memcheck, "ctest_memcheck") {
            result |= 0x10;
        }
    }
    if parts.contains("submit") {
        update_dashboard_configuration(&mut interp, &binary);
        if !invoke_dashboard_command(&mut interp, ctest_submit, "ctest_submit") {
            result |= 0x40;
        }
    }
    result
}
