//! `ctest --build-and-test` (cmCTestBuildAndTest).

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::time::{Duration, Instant};

use super::options::BuildAndTest;

fn write_line(stdout: &mut impl Write, line: &str, strip_status: bool) {
    let line = if strip_status && line.starts_with("-- ") {
        &line[3..]
    } else {
        line
    };
    let _ = stdout.write_all(line.as_bytes());
}

fn forward_output(
    mut reader: std::io::PipeReader,
    strip_status: bool,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut pending = Vec::new();
        let mut buffer = [0; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(len) => {
                    pending.extend_from_slice(&buffer[..len]);
                    let mut stdout = io::stdout().lock();
                    while let Some(newline) = pending.iter().position(|byte| *byte == b'\n') {
                        let line: Vec<u8> = pending.drain(..=newline).collect();
                        write_line(&mut stdout, &String::from_utf8_lossy(&line), strip_status);
                    }
                    let _ = stdout.flush();
                }
            }
        }
        if !pending.is_empty() {
            let mut stdout = io::stdout().lock();
            write_line(
                &mut stdout,
                &String::from_utf8_lossy(&pending),
                strip_status,
            );
            let _ = stdout.flush();
        }
    })
}

fn spawn_merged(
    mut command: Command,
    strip_status: bool,
) -> io::Result<(Child, std::thread::JoinHandle<()>)> {
    let (reader, writer) = io::pipe()?;
    command.stdout(writer.try_clone()?).stderr(writer);
    let child = command.spawn()?;
    command.stdout(std::process::Stdio::null());
    command.stderr(std::process::Stdio::null());
    Ok((child, forward_output(reader, strip_status)))
}

enum Wait {
    Exited(ExitStatus),
    TimedOut,
    SpawnError,
}

fn wait_child(mut child: Child, timeout: f64, output: std::thread::JoinHandle<()>) -> Wait {
    let started = Instant::now();
    let result = loop {
        if let Ok(Some(status)) = child.try_wait() {
            break Wait::Exited(status);
        }
        if timeout > 0.0 && started.elapsed().as_secs_f64() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            break Wait::TimedOut;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let _ = output.join();
    result
}

fn run_merged(command: Command, timeout: f64, strip_status: bool) -> Wait {
    match spawn_merged(command, strip_status) {
        Ok((child, output)) => wait_child(child, timeout, output),
        Err(_) => Wait::SpawnError,
    }
}

fn remaining(timeout: f64, started: Instant) -> Option<f64> {
    if timeout <= 0.0 {
        return Some(0.0);
    }
    let remaining = timeout - started.elapsed().as_secs_f64();
    (remaining > 0.0).then_some(remaining)
}

fn cmake_cache(binary: &Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(binary.join("CMakeCache.txt")).ok()?;
    let prefix = format!("{key}:FILEPATH=");
    text.lines()
        .find_map(|line| line.strip_prefix(&prefix).map(str::to_owned))
}

fn make_program(build: &BuildAndTest, binary: &Path) -> String {
    if !build.makeprogram.is_empty() {
        return build.makeprogram.clone();
    }
    cmake_cache(binary, "CMAKE_MAKE_PROGRAM").unwrap_or_default()
}

fn configure(build: &BuildAndTest, binary: &Path, config: &str) -> bool {
    let mut args = vec![
        super::sibling("cmake").to_string_lossy().into_owned(),
        build.source_dir.clone(),
    ];
    if !build.generator.is_empty() {
        args.push(format!("-G{}", build.generator));
    }
    if !build.platform.is_empty() {
        args.push(format!("-A{}", build.platform));
    }
    if !build.toolset.is_empty() {
        args.push(format!("-T{}", build.toolset));
    }
    if !config.is_empty() {
        args.push(format!("-DCMAKE_BUILD_TYPE:STRING={config}"));
    }
    if !build.makeprogram.is_empty()
        && (build.generator.contains("Make") || build.generator.contains("Ninja"))
    {
        args.push(format!(
            "-DCMAKE_MAKE_PROGRAM:FILEPATH={}",
            build.makeprogram
        ));
    }
    args.extend(build.options.iter().cloned());
    println!("======== CMake output     ======");
    let run = |args: &[String]| {
        let mut command = Command::new(&args[0]);
        command.args(&args[1..]).current_dir(binary);
        run_merged(command, 0.0, true)
    };
    let mut failed = !matches!(run(&args), Wait::Exited(status) if status.success());
    if !failed && build.two_config {
        failed = !matches!(run(&args), Wait::Exited(status) if status.success());
    }
    println!("======== End CMake output ======");
    if failed {
        println!("Error: cmake execution failed");
    }
    !failed
}

fn build_command_line(
    build: &BuildAndTest,
    binary: &Path,
    target: &str,
    config: &str,
    clean: bool,
) -> Vec<String> {
    let make = make_program(build, binary);
    let target = if clean {
        "clean".to_owned()
    } else {
        target.to_owned()
    };
    let mut command_line = vec![make];
    if build.generator == "Unix Makefiles" {
        command_line.extend(["-f".into(), "Makefile".into()]);
    } else if build.generator == "Ninja Multi-Config" && !config.is_empty() {
        command_line.extend(["-f".into(), format!("build-{config}.ninja")]);
    }
    if !target.is_empty() {
        command_line.push(target);
    }
    command_line
}

fn printable_command(command_line: &[String]) -> String {
    command_line
        .iter()
        .map(|part| {
            if part.is_empty() {
                "\"\"".to_owned()
            } else {
                part.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn build_target(
    build: &BuildAndTest,
    binary: &Path,
    target: &str,
    config: &str,
    clean: bool,
    timeout: f64,
) -> bool {
    let command_line = build_command_line(build, binary, target, config, clean);
    if command_line[0].is_empty() {
        eprintln!("no such file or directory");
        if clean {
            println!("CMake Error: Generator: execution of make clean failed.");
            println!("\nGenerator: execution of make clean failed.");
        } else {
            println!("CMake Error: Generator: build tool execution failed, command was:");
            println!("\nGenerator: build tool execution failed, command was:");
        }
        return false;
    }
    let mut command = Command::new(&command_line[0]);
    command.args(&command_line[1..]).current_dir(binary);
    let success =
        matches!(run_merged(command, timeout, false), Wait::Exited(status) if status.success());
    if !clean {
        println!();
    }
    success
}

fn candidate_paths(command_name: &str, config: &str, extra: &str) -> (Vec<String>, Vec<String>) {
    let (path, basename) = command_name
        .rsplit_once('/')
        .map_or((String::new(), command_name), |(path, name)| {
            (format!("{path}/"), name)
        });
    let mut attempted = Vec::new();
    let mut configs = Vec::new();
    let mut add = |path: &str, basename: &str| {
        attempted.push(format!("{path}{basename}"));
        configs.push(String::new());
        if !config.is_empty() {
            attempted.push(format!("{path}{config}/{basename}"));
            configs.push(config.to_owned());
            attempted.push(format!("{config}/{path}{basename}"));
            configs.push(config.to_owned());
        } else {
            for config in [
                "Release",
                "Debug",
                "MinSizeRel",
                "RelWithDebInfo",
                "Deployment",
                "Development",
            ] {
                attempted.push(format!("{path}{config}/{basename}"));
                configs.push(if config == "Development" {
                    "Deployment".into()
                } else {
                    config.into()
                });
            }
        }
    };
    add(&path, basename);
    if path.starts_with('/') {
        add(path.trim_start_matches('/'), basename);
    }
    if path.is_empty() && !extra.is_empty() {
        let extra_path = format!("{extra}/{command_name}");
        let (path, basename) = extra_path
            .rsplit_once('/')
            .map_or((String::new(), extra_path.as_str()), |(path, name)| {
                (format!("{path}/"), name)
            });
        add(&path, basename);
    }
    (attempted, configs)
}

fn find_executable(
    command_name: &str,
    binary: &Path,
    config: &str,
    extra: &str,
) -> (PathBuf, String, Vec<String>) {
    let (attempted, configs) = candidate_paths(command_name, config, extra);
    let mut failed = Vec::new();
    for (candidate, resulting) in attempted.iter().zip(configs) {
        let path = binary.join(candidate);
        if path.is_file() {
            return (path, resulting, failed);
        }
        failed.push(candidate.clone());
        failed.push(candidate.clone());
    }
    if !command_name.contains('/') {
        if let Some(paths) = std::env::var_os("PATH") {
            for entry in std::env::split_paths(&paths) {
                let candidate = entry.join(command_name);
                if candidate.is_file() {
                    return (candidate, String::new(), failed);
                }
            }
        }
    }
    (PathBuf::new(), String::new(), failed)
}

pub(super) fn run(build: &BuildAndTest, config_type: &str) -> i32 {
    if build.generator.is_empty() {
        println!(
            "--build-and-test requires that the generator be provided using the --build-generator command line option."
        );
        return 1;
    }
    let mut config = config_type.to_owned();
    let binary = PathBuf::from(&build.binary_dir);
    if config.is_empty() && !build.config_sample.is_empty() {
        let current = std::env::current_dir().unwrap_or_default();
        let (sample, resulting, _) = find_executable(&build.config_sample, &current, &config, "");
        if !sample.as_os_str().is_empty() && !resulting.is_empty() {
            config = resulting.clone();
        }
        println!(
            "Using config sample with results: {} and {}",
            sample.display(),
            resulting
        );
    }
    let started = Instant::now();
    println!(
        "Internal cmake changing into directory: {}",
        build.binary_dir
    );
    if !binary.is_dir() {
        let _ = std::fs::create_dir_all(&binary);
    }
    if let Err(error) = std::env::set_current_dir(&binary) {
        println!(
            "Failed to change working directory to \"{}\": {}",
            build.binary_dir,
            error
                .to_string()
                .split(" (os error")
                .next()
                .unwrap_or_default()
        );
        return 1;
    }

    if !build.no_cmake && !configure(build, &binary, &config) {
        return 1;
    }

    let build_config = if config.is_empty() { "Debug" } else { &config };
    let targets = if build.targets.is_empty() {
        vec![String::new()]
    } else {
        build.targets.clone()
    };
    for target in targets {
        let Some(remaining_time) = remaining(build.timeout, started) else {
            print!("--build-and-test timeout exceeded. ");
            return 1;
        };
        println!("Change Dir: '{}'", build.binary_dir);
        if !build.no_clean {
            let clean = build_command_line(build, &binary, "", build_config, true);
            println!("\nRun Clean Command: {}", printable_command(&clean));
            if !build_target(build, &binary, "", build_config, true, remaining_time) {
                return 1;
            }
        }
        let build_command = build_command_line(build, &binary, &target, build_config, false);
        println!(
            "\nRun Build Command(s): {}",
            printable_command(&build_command)
        );
        if !build_target(build, &binary, &target, build_config, false, remaining_time) {
            return 1;
        }
    }

    if build.test_command.is_empty() {
        return 0;
    }
    let (executable, _, failed) =
        find_executable(&build.test_command, &binary, &config, &build.exe_dir);
    if executable.as_os_str().is_empty() || !executable.is_file() {
        println!(
            "Could not find path to executable, perhaps it was not built: {}",
            build.test_command
        );
        println!("tried to find it in these places:");
        println!("{}", executable.display());
        for fail in failed {
            println!("{fail}");
        }
        return 1;
    }
    let mut run_dir = binary.clone();
    if !build.run_dir.is_empty() {
        println!("Run test in directory: {}", build.run_dir);
        run_dir = PathBuf::from(&build.run_dir);
        if run_dir.is_relative() {
            run_dir = binary.join(run_dir);
        }
        if !run_dir.is_dir() || std::env::set_current_dir(&run_dir).is_err() {
            println!(
                "Failed to change working directory to \"{}\": No such file or directory",
                build.run_dir
            );
            return 1;
        }
    }
    print!("Running test command: \"{}\"", executable.display());
    for arg in &build.test_args {
        print!(" \"{arg}\"");
    }
    println!();
    let _ = io::stdout().flush();
    let Some(remaining_time) = remaining(build.timeout, started) else {
        print!("--build-and-test timeout exceeded. ");
        return 1;
    };
    let mut command = Command::new(&executable);
    command.args(&build.test_args).current_dir(&run_dir);
    match run_merged(command, remaining_time, false) {
        Wait::Exited(status) if status.success() => 0,
        Wait::Exited(_) | Wait::TimedOut => {
            println!("\nTest command failed: {}", executable.display());
            1
        }
        Wait::SpawnError => {
            println!("\n*** ERROR executing: {}", executable.display());
            println!("\nTest command failed: {}", executable.display());
            1
        }
    }
}
