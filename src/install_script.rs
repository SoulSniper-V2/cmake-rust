//! `cmake --install` script selection and execution
//! (`cmInstallScriptHandler`): one top-level `cmake_install.cmake`, or with
//! INSTALL_PARALLEL every directory's script run as a separate
//! `cmake -P` process, `-j` at a time.

use std::collections::VecDeque;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

pub struct InstallScriptHandler {
    binary_dir: String,
    component: String,
    parallel: bool,
    commands: Vec<Vec<String>>,
    directories: Vec<String>,
}

/// kwsys `FileTimeCompare`: -1, 0 or 1 by modification time (nanoseconds).
fn file_time_compare(a: &str, b: &str) -> Option<i32> {
    use std::os::unix::fs::MetadataExt;
    let a = std::fs::metadata(a).ok()?;
    let b = std::fs::metadata(b).ok()?;
    let ta = (a.mtime(), a.mtime_nsec());
    let tb = (b.mtime(), b.mtime_nsec());
    Some(match ta.cmp(&tb) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    })
}

/// `cmSystemTools::RelativePath(from_dir, to)`.
fn relative_path(from_dir: &str, to: &str) -> String {
    let from: Vec<&str> = from_dir.split('/').filter(|p| !p.is_empty()).collect();
    let to_parts: Vec<&str> = to.split('/').filter(|p| !p.is_empty()).collect();
    let common = from
        .iter()
        .zip(&to_parts)
        .take_while(|(a, b)| a == b)
        .count();
    let mut parts: Vec<&str> = std::iter::repeat_n("..", from.len() - common).collect();
    parts.extend(&to_parts[common..]);
    parts.join("/")
}

impl InstallScriptHandler {
    /// The constructor: `args` is the base command line ending in `-P`.
    pub fn new(binary_dir: &str, component: &str, config: &str, args: &mut Vec<String>) -> Self {
        let mut handler = Self {
            binary_dir: binary_dir.to_owned(),
            component: component.to_owned(),
            parallel: false,
            commands: Vec::new(),
            directories: Vec::new(),
        };
        let file = format!("{binary_dir}/CMakeFiles/InstallScripts.json");
        let mut compare = 1;
        if Path::new(&file).exists()
            && let Some(result) =
                file_time_compare(&format!("{binary_dir}/CMakeFiles/cmake.check_cache"), &file)
        {
            compare = result;
        }
        if compare < 1 {
            let value: serde_json::Value = std::fs::read_to_string(&file)
                .ok()
                .and_then(|text| serde_json::from_str(&text).ok())
                .unwrap_or(serde_json::Value::Null);
            handler.parallel = value["Parallel"].as_bool().unwrap_or(false);
            if handler.parallel {
                let at = args.len() - 1;
                args.insert(at, "-DCMAKE_INSTALL_LOCAL_ONLY=1".to_owned());
            }
            let configs: Vec<String> = if config.is_empty()
                && let Some(configs) = value.get("Configs").and_then(|c| c.as_array())
            {
                configs
                    .iter()
                    .map(|c| c.as_str().unwrap_or_default().to_owned())
                    .collect()
            } else {
                vec![config.to_owned()]
            };
            if let Some(scripts) = value["InstallScripts"].as_array() {
                for script in scripts {
                    let script = script.as_str().unwrap_or_default();
                    for config in &configs {
                        handler.add_script(args, script, config);
                    }
                    if !handler.parallel {
                        break;
                    }
                }
            }
        } else {
            let script = format!("{binary_dir}/cmake_install.cmake");
            handler.add_script(args, &script, config);
        }
        handler
    }

    fn add_script(&mut self, args: &[String], script: &str, config: &str) {
        let mut command = args.to_vec();
        if !config.is_empty() {
            let at = command.len() - 1;
            command.insert(at, format!("-DCMAKE_INSTALL_CONFIG_NAME={config}"));
        }
        command.push(script.to_owned());
        self.commands.push(command);
        self.directories.push(crate::util::filename_path(script));
    }

    pub fn is_parallel(&self) -> bool {
        self.parallel
    }

    /// The `instrument_arg` prefix `Install` puts before every command.
    pub fn prefix_commands(&mut self, prefix: &[String]) {
        for command in &mut self.commands {
            command.splice(0..0, prefix.iter().cloned());
        }
    }

    pub fn commands(&self) -> &[Vec<String>] {
        &self.commands
    }

    /// `cmInstallScriptHandler::Install`: runs the scripts `jobs` at a time,
    /// printing each one's merged output when it finishes, then writes the
    /// install manifest from the per-directory local manifests.
    pub fn install(&self, jobs: u32, cwd: &str) -> i32 {
        let jobs = jobs.max(1) as usize;
        let total = self.commands.len();
        let mut pending: VecDeque<usize> = (0..total).collect();
        let mut running: Vec<(usize, std::process::Child, std::thread::JoinHandle<Vec<u8>>)> =
            Vec::new();
        let mut installed = 0;
        let name_of = |index: usize| {
            relative_path(cwd, self.commands[index].last().map_or("", String::as_str))
        };
        loop {
            while running.len() < jobs
                && let Some(index) = pending.pop_front()
            {
                let command = &self.commands[index];
                // SetMergedBuiltinStreams: one pipe for stdout and stderr.
                let Ok((reader, writer)) = std::io::pipe() else {
                    continue;
                };
                let Ok(writer2) = writer.try_clone() else {
                    continue;
                };
                let mut cmd = Command::new(&command[0]);
                cmd.args(&command[1..])
                    .stdout(writer)
                    .stderr(writer2)
                    .stdin(Stdio::null());
                let Ok(child) = cmd.spawn() else {
                    installed += 1;
                    println!("[{installed}/{total}] {}", name_of(index));
                    continue;
                };
                drop(cmd);
                let mut reader = reader;
                let thread = std::thread::spawn(move || {
                    let mut data = Vec::new();
                    let _ = reader.read_to_end(&mut data);
                    data
                });
                running.push((index, child, thread));
            }
            if running.is_empty() {
                break;
            }
            // Report whichever script finished first.
            let mut finished = None;
            while finished.is_none() {
                for (slot, (_, child, _)) in running.iter_mut().enumerate() {
                    if matches!(child.try_wait(), Ok(Some(_))) {
                        finished = Some(slot);
                        break;
                    }
                }
                if finished.is_none() {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
            let (index, _child, thread) = running.remove(finished.unwrap());
            let output = thread.join().unwrap_or_default();
            installed += 1;
            // InstallScript::printResult
            print!(
                "[{installed}/{total}] {}\n{}",
                name_of(index),
                crate::bytes::decode(&output)
            );
        }
        let manifest = if self.component.is_empty() {
            "install_manifest.txt".to_owned()
        } else if self
            .component
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'+' | b'-'))
        {
            format!("install_manifest_{}.txt", self.component)
        } else {
            format!(
                "install_manifest_{}.txt",
                crate::commands::string::misc::hash_string("MD5", &self.component)
            )
        };
        let mut content = String::new();
        for dir in &self.directories {
            if let Ok(text) = std::fs::read_to_string(format!("{dir}/install_local_manifest.txt")) {
                for line in text.lines() {
                    content.push_str(line);
                    content.push('\n');
                }
            }
        }
        // cmGeneratedFileStream with SetCopyIfDifferent(true).
        let _ = crate::fsutil::write_if_different(
            Path::new(&format!("{}/{manifest}", self.binary_dir)),
            &content,
        );
        0
    }
}
