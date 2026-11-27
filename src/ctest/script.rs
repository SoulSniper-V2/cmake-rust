//! CTest dashboard script execution (`ctest -S`).

use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::interp::{CmdResult, Interp, err};
use crate::util::collapse_full_path;

use super::options::{Options, integer};
use super::{Log, Show, execute_detailed, report, testfile};

#[derive(Clone, Debug)]
pub(crate) struct State {
    options: Options,
    source_dir: String,
    binary_dir: String,
    model: String,
    group: String,
    tag: String,
    site: String,
    build_name: String,
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
            tag: String::new(),
            site: hostname(),
            build_name: String::new(),
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
    let tag = if append {
        let tag_file = PathBuf::from(&binary_dir).join("Testing/TAG");
        std::fs::read_to_string(tag_file)
            .ok()
            .and_then(|text| text.lines().next().map(str::to_owned))
            .unwrap_or_else(|| report::clock("%Y%m%d-%H%M"))
    } else {
        report::clock("%Y%m%d-%H%M")
    };
    let model_name = model.clone().unwrap_or_default();
    let group_name = group.clone().unwrap_or_else(|| model_name.clone());
    {
        let state = interp.ext.ctest_script.as_mut().unwrap();
        state.source_dir = source_dir;
        state.binary_dir = binary_dir.clone();
        state.model = model_name.clone();
        state.group = group_name.clone();
        state.tag = tag.clone();
        if !site.is_empty() {
            state.site = site;
        }
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
            interp.ext.ctest_script.as_ref().unwrap().build_name
        ),
        quiet,
    );
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
    let _ = std::fs::write(
        testing.join("TAG"),
        format!("{tag}\n{model_name}\n{group_name}\n"),
    );
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
    if !options.extra_verbose {
        options.verbose = false;
    }
    if options.timeout <= 0.0 {
        options.timeout = interp
            .get_def("CTEST_TEST_TIMEOUT")
            .map_or(600.0, super::options::number);
    }
    options
}

fn ctest_test(interp: &mut Interp, args: &[String]) -> CmdResult {
    let mut build = None;
    let mut return_value = None;
    let mut quiet = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "BUILD" | "RETURN_VALUE" | "PARALLEL_LEVEL" | "INCLUDE" | "EXCLUDE"
            | "INCLUDE_LABEL" | "EXCLUDE_LABEL" | "TEST_LOAD" | "STOP_TIME" | "SCHEDULE_RANDOM"
            | "REPEAT" | "RESOURCE_SPEC_FILE" | "OUTPUT_JUNIT" => {
                let key = args[index].clone();
                index += 1;
                if index >= args.len() {
                    return err(format!("{key} argument missing value"));
                }
                match key.as_str() {
                    "BUILD" => build = Some(args[index].clone()),
                    "RETURN_VALUE" => return_value = Some(args[index].clone()),
                    _ => {}
                }
            }
            "QUIET" => quiet = true,
            "APPEND" | "STOP_ON_FAILURE" => {}
            value if build.is_none() => build = Some(value.to_owned()),
            value => return err(format!("called with unknown argument \"{value}\".")),
        }
        index += 1;
    }
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
                state.source_dir
            ),
            false,
        );
    }
    let options = test_options(interp, &binary);
    let mut log = Log::new(&options);
    log.suppress_output = !handler_verbose || quiet;
    if !binary.is_dir() {
        let error = super::directory_error(&binary);
        handler_error(interp, &format!("{error}\n"));
        interp.ext.ctest_script.as_mut().unwrap().had_error = true;
        return Ok(());
    }
    let old_dir = std::env::current_dir().unwrap_or_default();
    let (code, no_tests) = execute_detailed(binary, options, &mut log);
    let _ = std::env::set_current_dir(old_dir);
    if let Some(variable) = return_value {
        interp.set_def(variable, code.to_string());
    }
    if no_tests {
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

fn ctest_empty_binary_directory(_interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() != 1 {
        return err("called with incorrect number of arguments");
    }
    let path = PathBuf::from(&args[0]);
    if args[0].len() < 2 {
        return err(format!(
            "Did not remove the binary directory:\n {}\nbecause:\n path too short",
            args[0]
        ));
    }
    if !path.exists() {
        return Ok(());
    }
    if !path.join("CMakeCache.txt").exists() {
        return err(format!(
            "Did not remove the binary directory:\n {}\nbecause:\n path does not contain an existing CMakeCache.txt file",
            args[0]
        ));
    }
    if std::fs::remove_dir_all(path).is_err() {
        return err(format!(
            "Did not remove the binary directory:\n {}\nbecause:\n removal failed",
            args[0]
        ));
    }
    Ok(())
}

fn read_custom_tree(interp: &mut Interp, dir: &Path) {
    let mut current = Some(dir);
    while let Some(dir) = current {
        for name in ["CTestCustom.cmake", "CTestCustom.ctest"] {
            let file = dir.join(name);
            if file.is_file() {
                let _ = interp.read_list_file(&file.to_string_lossy());
            }
        }
        current = dir.parent();
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

fn unsupported(_: &mut Interp, _: &[String]) -> CmdResult {
    err("ctest command is not implemented")
}

fn register(interp: &mut Interp) {
    interp.register("ctest_start", ctest_start);
    interp.register("ctest_test", ctest_test);
    interp.register("ctest_sleep", ctest_sleep);
    interp.register("ctest_empty_binary_directory", ctest_empty_binary_directory);
    interp.register("ctest_read_custom_files", ctest_read_custom_files);
    interp.register("ctest_run_script", ctest_run_script);
    interp.register("ctest_build", unsupported);
    interp.register("ctest_configure", unsupported);
    interp.register("ctest_coverage", unsupported);
    interp.register("ctest_memcheck", unsupported);
    interp.register("ctest_submit", unsupported);
    interp.register("ctest_update", unsupported);
    interp.register("ctest_upload", unsupported);
}

fn run_one(total_script: &str, _in_process: bool, options: &Options) -> i32 {
    let (script, script_arg) = total_script
        .split_once(',')
        .map_or((total_script, ""), |(script, arg)| (script, arg));
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
