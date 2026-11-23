//! `cmQtAutoGenerator`: logging, info-file (JSON) access, file helpers and
//! the `Run` scaffold shared by the moc/uic and rcc generators.

use std::collections::BTreeSet;
use std::io::Write as _;
use std::os::unix::io::FromRawFd as _;
use std::process::{Command, Stdio};

use super::filetime::FileTime;
use super::{GenT, quoted, quoted_command, str_to_ulong};

/// `cmSystemTools::Stdout`.
fn stdout(text: &str) {
    print!("{text}");
    let _ = std::io::stdout().flush();
}

/// `cmSystemTools::Stderr`.
fn stderr(text: &str) {
    eprint!("{text}");
    let _ = std::io::stderr().flush();
}

/// `cmQtAutoGenerator::Logger`.
pub(crate) struct Logger {
    verbosity: u32,
}

impl Logger {
    pub(crate) fn new() -> Logger {
        let mut logger = Logger { verbosity: 0 };
        if let Ok(verbose) = std::env::var("VERBOSE")
            && !verbose.is_empty()
        {
            match str_to_ulong(&verbose) {
                Some(value) => logger.verbosity = value as u32,
                None => logger.verbosity = u32::from(crate::util::is_on(&verbose)),
            }
        }
        logger
    }

    pub(crate) fn verbose(&self) -> bool {
        self.verbosity != 0
    }

    /// `RaiseVerbosity`.
    pub(crate) fn raise_verbosity(&mut self, value: u32) {
        self.verbosity = self.verbosity.max(value);
    }

    /// `HeadLine`.
    fn headline(title: &str) -> String {
        format!("{title}\n{}\n", "-".repeat(title.len()))
    }

    /// `Logger::Info`.
    pub(crate) fn info(&self, gen_type: GenT, message: &str) {
        stdout(&format!(
            "{}: {}{}",
            gen_type.name(),
            message,
            if message.ends_with('\n') { "" } else { "\n" }
        ));
    }

    /// `Logger::Warning`.
    pub(crate) fn warning(&self, gen_type: GenT, message: &str) {
        let tail = "\n\n";
        let tail = if message.ends_with('\n') { "\n" } else { tail };
        let msg = if !message.contains('\n') {
            format!("{} warning: {message}{tail}", gen_type.name())
        } else {
            format!(
                "{}{message}{tail}",
                Self::headline(&format!("{} warning", gen_type.name()))
            )
        };
        stdout(&msg);
    }

    /// `Logger::Error`.
    pub(crate) fn error(&self, gen_type: GenT, message: &str) {
        let tail = if message.ends_with('\n') {
            "\n"
        } else {
            "\n\n"
        };
        stderr(&format!(
            "\n{}{message}{tail}",
            Self::headline(&format!("{} error", gen_type.name()))
        ));
    }

    /// `Logger::ErrorCommand`.
    pub(crate) fn error_command(
        &self,
        gen_type: GenT,
        message: &str,
        command: &[String],
        output: &str,
    ) {
        let tail = if message.ends_with('\n') {
            "\n"
        } else {
            "\n\n"
        };
        let mut msg = format!(
            "\n{}{message}{tail}",
            Self::headline(&format!("{} subprocess error", gen_type.name()))
        );
        msg.push_str(&format!(
            "{}{}\n\n",
            Self::headline("Command"),
            quoted_command(command)
        ));
        msg.push_str(&format!(
            "{}{}{}",
            Self::headline("Output"),
            output,
            if output.ends_with('\n') { "\n" } else { "\n\n" }
        ));
        stderr(&msg);
    }
}

/// `cmQtAutoGenerator::ProjectDirsT`.
#[derive(Default)]
pub(crate) struct ProjectDirs {
    pub(crate) source: String,
    pub(crate) binary: String,
    pub(crate) current_source: String,
    pub(crate) current_binary: String,
}

/// `cmQtAutoGenerator::MakeParentDirectory`.
pub(crate) fn make_parent_directory(filename: &str) -> bool {
    let dir = crate::util::filename_path(filename);
    dir.is_empty() || std::fs::create_dir_all(&dir).is_ok()
}

/// `cmQtAutoGenerator::FileRead` (binary, whole file).
pub(crate) fn file_read(filename: &str, error: Option<&mut String>) -> Option<Vec<u8>> {
    fn fail(error: Option<&mut String>, text: &str) -> Option<Vec<u8>> {
        if let Some(error) = error {
            *error = text.to_owned();
        }
        None
    }
    if !std::path::Path::new(filename).is_file() {
        return fail(error, "Not a file.");
    }
    match std::fs::read(filename) {
        Ok(content) => Some(content),
        Err(_) => fail(error, "Opening the file for reading failed."),
    }
}

/// `cmQtAutoGenerator::FileWrite`.
pub(crate) fn file_write(filename: &str, content: &[u8], error: Option<&mut String>) -> bool {
    fn fail(error: Option<&mut String>, text: &str) -> bool {
        if let Some(error) = error {
            *error = text.to_owned();
        }
        false
    }
    if !make_parent_directory(filename) {
        return fail(error, "Could not create parent directory.");
    }
    match std::fs::write(filename, content) {
        Ok(()) => true,
        Err(_) => fail(error, "Opening file for writing failed."),
    }
}

/// `cmQtAutoGenerator::FileDiffers`.
pub(crate) fn file_differs(filename: &str, content: &[u8]) -> bool {
    file_read(filename, None) != Some(content.to_vec())
}

/// `cmQtAutoGenerator::SettingsFind`.
pub(crate) fn settings_find(content: &str, key: &str) -> String {
    let prefix = format!("{key}:");
    let Some(pos) = content.find(&prefix) else {
        return String::new();
    };
    let pos = pos + prefix.len();
    if pos < content.len()
        && let Some(end) = content[pos..].find('\n')
        && end != 0
    {
        return content[pos..pos + end].to_owned();
    }
    String::new()
}

/// `cmSystemTools::Touch`: `utimensat`; `create` creates a missing file.
pub(crate) fn touch(filename: &str, create: bool) -> bool {
    if !std::path::Path::new(filename).exists() {
        if create {
            return std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(filename)
                .is_ok();
        }
        return true;
    }
    let Ok(c_path) = std::ffi::CString::new(filename) else {
        return false;
    };
    // SAFETY: c_path is a valid NUL-terminated string; nullptr times means now.
    unsafe { libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), std::ptr::null(), 0) == 0 }
}

/// `cmWorkerPool::ProcessResultT`.
#[derive(Default)]
pub(crate) struct ProcessResult {
    pub(crate) stdout: Vec<u8>,
    pub(crate) error_message: String,
}

impl ProcessResult {
    pub(crate) fn error(&self) -> bool {
        !self.error_message.is_empty()
    }
}

/// `cmWorkerPoolWorker::RunProcess`: run `command` in `dir` with stdout
/// and stderr merged into `result.stdout` (single pipe, arrival order).
pub(crate) fn run_process(command: &[String], dir: &str) -> ProcessResult {
    let mut result = ProcessResult::default();
    if command.is_empty() {
        return result;
    }
    let mut pipe_fds = [0 as libc::c_int; 2];
    // SAFETY: pipe initializes both fds on success.
    if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } != 0 {
        result.error_message = "libuv stdout pipe initialization failed".to_owned();
        return result;
    }
    // SAFETY: pipe_fds were just created by pipe.
    let (read, write) = unsafe {
        (
            std::fs::File::from_raw_fd(pipe_fds[0]),
            std::fs::File::from_raw_fd(pipe_fds[1]),
        )
    };
    let write_err = match write.try_clone() {
        Ok(file) => file,
        Err(_) => {
            result.error_message = "libuv stderr pipe initialization failed".to_owned();
            return result;
        }
    };
    let spawn = Command::new(&command[0])
        .args(&command[1..])
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(write))
        .stderr(Stdio::from(write_err))
        .spawn();
    let mut child = match spawn {
        Ok(child) => child,
        Err(err) => {
            result.error_message = format!(
                "libuv process spawn failed: {}",
                super::super::process_error(&err)
            );
            return result;
        }
    };
    // The child's write ends were moved into it; drain the pipe until EOF.
    let mut read = read;
    let _ = std::io::Read::read_to_end(&mut read, &mut result.stdout);
    match child.wait() {
        Ok(status) => {
            if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&status) {
                result.error_message = format!("Process was terminated by signal {signal}");
            } else if !status.success() {
                result.error_message = format!(
                    "Process failed with return value {}",
                    status.code().unwrap_or(1)
                );
            }
        }
        Err(_) => {
            result.error_message = "Process wait failed".to_owned();
        }
    }
    result
}

/// `cmSystemTools::RunSingleCommand` (OUTPUT_NONE): separate captures.
pub(crate) struct SingleResult {
    pub(crate) ok: bool,
    pub(crate) ret_val: i32,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

pub(crate) fn run_single_command(command: &[String], dir: &str) -> SingleResult {
    match Command::new(&command[0])
        .args(&command[1..])
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) => SingleResult {
            ok: true,
            ret_val: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        },
        Err(_) => SingleResult {
            ok: false,
            ret_val: 0,
            stdout: String::new(),
            stderr: String::new(),
        },
    }
}

/// `cmFileLock` on POSIX: `fcntl` `F_WRLCK`, `F_SETLKW` without timeout.
pub(crate) struct FileLock {
    fd: Option<std::fs::File>,
}

impl FileLock {
    pub(crate) fn new() -> FileLock {
        FileLock { fd: None }
    }

    /// `Lock(filename, (unsigned long)-1)`: blocks until the lock is taken.
    /// Returns an error string like `cmFileLockResult::GetOutputMessage`.
    pub(crate) fn lock(&mut self, filename: &str) -> Result<(), String> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(filename)
            .map_err(|err| super::super::os_error(&err))?;
        let fd = std::os::unix::io::AsRawFd::as_raw_fd(&file);
        let mut flock: libc::flock = unsafe { std::mem::zeroed() };
        flock.l_type = libc::F_WRLCK as libc::c_short;
        flock.l_whence = libc::SEEK_SET as libc::c_short;
        // SAFETY: flock is a valid pointer; fd is open.
        if unsafe { libc::fcntl(fd, libc::F_SETLKW, &flock) } == -1 {
            return Err(super::super::os_error(&std::io::Error::last_os_error()));
        }
        self.fd = Some(file);
        Ok(())
    }

    /// `Release`.
    pub(crate) fn release(&mut self) {
        if let Some(file) = self.fd.take() {
            let fd = std::os::unix::io::AsRawFd::as_raw_fd(&file);
            let mut flock: libc::flock = unsafe { std::mem::zeroed() };
            flock.l_type = libc::F_UNLCK as libc::c_short;
            flock.l_whence = libc::SEEK_SET as libc::c_short;
            // SAFETY: flock is a valid pointer.
            unsafe { libc::fcntl(fd, libc::F_SETLK, &flock) };
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        self.release();
    }
}

/// `cmQtAutoGenerator::InfoT`: JSON object view of the info file with
/// config-suffixed key lookup.  Borrows only the generator fields it
/// reads so other fields can be filled while it is alive.
pub(crate) struct Info<'a> {
    json: &'a serde_json::Value,
    info_config: &'a str,
    info_file: &'a str,
    gen_type: GenT,
    log: &'a Logger,
}

/// `Json_[key]` semantics: a missing key and an explicit `null` are the
/// same `Json::nullValue`.
fn json_get<'a>(json: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
    match json.get(key) {
        Some(serde_json::Value::Null) | None => None,
        other => other,
    }
}

impl<'a> Info<'a> {
    pub(crate) fn new(
        json: &'a serde_json::Value,
        info_config: &'a str,
        info_file: &'a str,
        gen_type: GenT,
        log: &'a Logger,
    ) -> Info<'a> {
        Info {
            json,
            info_config,
            info_file,
            gen_type,
            log,
        }
    }

    /// `GetValue`: returns the raw JSON value (null for missing).
    pub(crate) fn value(&self, key: &str) -> &serde_json::Value {
        json_get(self.json, key).unwrap_or(&serde_json::Value::Null)
    }

    fn config_key(&self, key: &str) -> String {
        format!("{}_{}", key, self.info_config)
    }

    /// `LogError` for the generator.
    pub(crate) fn log_error(&self, message: String) -> bool {
        self.log.error(
            self.gen_type,
            &format!(
                "Info error in info file\n{}:\n{message}",
                quoted(self.info_file)
            ),
        );
        false
    }

    /// `GetString`.
    pub(crate) fn get_string(&self, key: &str, value: &mut String, required: bool) -> bool {
        match json_get(self.json, key) {
            Some(serde_json::Value::String(s)) => {
                *value = s.clone();
                if value.is_empty() && required {
                    return self.log_error(format!("{key} is empty."));
                }
            }
            Some(_) => {
                return self.log_error(format!("{key} is not a string."));
            }
            None => {
                if required {
                    return self.log_error(format!("{key} is not a string."));
                }
            }
        }
        true
    }

    /// `GetStringConfig`: try `key_<CONFIG>` then `key`.
    pub(crate) fn get_string_config(&self, key: &str, value: &mut String, required: bool) -> bool {
        let config_key = self.config_key(key);
        if let Some(jval) = json_get(self.json, &config_key) {
            let serde_json::Value::String(s) = jval else {
                return self.log_error(format!("{config_key} is not a string."));
            };
            *value = s.clone();
            if required && value.is_empty() {
                return self.log_error(format!("{config_key} is empty."));
            }
            return true;
        }
        self.get_string(key, value, required)
    }

    /// `GetBool`.
    pub(crate) fn get_bool(&self, key: &str, value: &mut bool, required: bool) -> bool {
        match json_get(self.json, key) {
            Some(serde_json::Value::Bool(b)) => *value = *b,
            Some(_) => return self.log_error(format!("{key} is not a boolean.")),
            None => {
                if required {
                    return self.log_error(format!("{key} is not a boolean."));
                }
            }
        }
        true
    }

    /// `GetUInt` (`isUInt` = non-negative integer).
    pub(crate) fn get_uint(&self, key: &str, value: &mut u32, required: bool) -> bool {
        match json_get(self.json, key) {
            Some(jval) if jval.is_u64() => *value = jval.as_u64().unwrap_or_default() as u32,
            Some(_) => return self.log_error(format!("{key} is not an unsigned integer.")),
            None => {
                if required {
                    return self.log_error(format!("{key} is not an unsigned integer."));
                }
            }
        }
        true
    }

    /// `GetJsonArray` into a vector (returns false when nothing appended).
    pub(crate) fn json_array_vec(list: &mut Vec<String>, jval: &serde_json::Value) -> bool {
        let Some(array) = jval.as_array() else {
            return false;
        };
        let mut picked = false;
        for item in array {
            if let Some(s) = item.as_str() {
                list.push(s.to_owned());
                picked = true;
            }
        }
        picked
    }

    /// `GetArray` (vector form).
    pub(crate) fn get_array(&self, key: &str, list: &mut Vec<String>, required: bool) -> bool {
        match json_get(self.json, key) {
            Some(jval) if jval.is_array() => Self::json_array_vec(list, jval) || !required,
            Some(_) => self.log_error(format!("{key} is not an array.")),
            None => {
                if required {
                    self.log_error(format!("{key} is not an array."))
                } else {
                    true
                }
            }
        }
    }

    /// `GetArray` (set form; skips non-strings like upstream).
    pub(crate) fn get_array_set(
        &self,
        key: &str,
        list: &mut BTreeSet<String>,
        required: bool,
    ) -> bool {
        match json_get(self.json, key) {
            Some(jval) if jval.is_array() => {
                let mut picked = false;
                for item in jval.as_array().map_or(&[][..], Vec::as_slice) {
                    if let Some(s) = item.as_str() {
                        list.insert(s.to_owned());
                        picked = true;
                    }
                }
                picked || !required
            }
            Some(_) => self.log_error(format!("{key} is not an array.")),
            None => {
                if required {
                    self.log_error(format!("{key} is not an array."))
                } else {
                    true
                }
            }
        }
    }

    /// `GetArrayConfig`: try `key_<CONFIG>` then `key`.
    pub(crate) fn get_array_config(
        &self,
        key: &str,
        list: &mut Vec<String>,
        required: bool,
    ) -> bool {
        let config_key = self.config_key(key);
        if let Some(jval) = json_get(self.json, &config_key) {
            if !jval.is_array() {
                return self.log_error(format!("{config_key} is not an array string."));
            }
            if !Self::json_array_vec(list, jval) && required {
                return self.log_error(format!("{config_key} is empty."));
            }
            return true;
        }
        self.get_array(key, list, required)
    }
}

/// `cmQtAutoGenerator` state common to both generators.
pub(crate) struct GeneratorBase {
    pub(crate) gen_type: GenT,
    pub(crate) log: Logger,
    pub(crate) info_file: String,
    pub(crate) info_dir: String,
    pub(crate) info_file_time: FileTime,
    pub(crate) info_config: String,
    pub(crate) executable_config: String,
    pub(crate) project_dirs: ProjectDirs,
}

impl GeneratorBase {
    pub(crate) fn new(gen_type: GenT) -> GeneratorBase {
        GeneratorBase {
            gen_type,
            log: Logger::new(),
            info_file: String::new(),
            info_dir: String::new(),
            info_file_time: FileTime::default(),
            info_config: String::new(),
            executable_config: String::new(),
            project_dirs: ProjectDirs::default(),
        }
    }

    /// `MessagePath`: shorten to `SRC:`/`BIN:` relative, then quote.
    pub(crate) fn message_path(&self, path: &str) -> String {
        let dirs = &self.project_dirs;
        let res = if path.starts_with(&dirs.source) {
            format!("SRC:{}", &path[dirs.source.len()..])
        } else if path.starts_with(&dirs.binary) {
            format!("BIN:{}", &path[dirs.binary.len()..])
        } else {
            path.to_owned()
        };
        quoted(&res)
    }

    /// `cmQtAutoGenerator::Run` up to `InitFromInfo`: info config/file,
    /// JSON parse and common keys. Returns the parsed info JSON.
    pub(crate) fn run_init(
        &mut self,
        info_file: &str,
        config: &str,
        executable_config: &str,
    ) -> Option<serde_json::Value> {
        self.info_config = config.to_owned();
        self.executable_config = executable_config.to_owned();
        self.info_file = info_file.to_owned();
        self.info_dir = crate::util::filename_path(info_file);

        // Load info file time
        self.info_file_time = FileTime::load(info_file);
        if !self.info_file_time.exists() {
            stderr(&format!(
                "AutoGen: The info file {} is not readable\n",
                quoted(&self.info_file)
            ));
            return None;
        }

        // Read info file
        let json = match std::fs::File::open(info_file) {
            Ok(mut file) => {
                let mut text = String::new();
                if std::io::Read::read_to_string(&mut file, &mut text).is_err() {
                    self.log.error(
                        self.gen_type,
                        &format!("Could not to open info file {}", quoted(&self.info_file)),
                    );
                    return None;
                }
                match serde_json::from_str::<serde_json::Value>(&text) {
                    Ok(json) => json,
                    Err(_) => {
                        self.log.error(
                            self.gen_type,
                            &format!("Could not read info file {}", quoted(&self.info_file)),
                        );
                        return None;
                    }
                }
            }
            Err(_) => {
                self.log.error(
                    self.gen_type,
                    &format!("Could not to open info file {}", quoted(&self.info_file)),
                );
                return None;
            }
        };

        // Read common info settings
        let info = Info::new(
            &json,
            &self.info_config,
            &self.info_file,
            self.gen_type,
            &self.log,
        );
        let mut verbosity = 0u32;
        if !info.get_uint("VERBOSITY", &mut verbosity, false)
            || !info.get_string("CMAKE_SOURCE_DIR", &mut self.project_dirs.source, true)
            || !info.get_string("CMAKE_BINARY_DIR", &mut self.project_dirs.binary, true)
            || !info.get_string(
                "CMAKE_CURRENT_SOURCE_DIR",
                &mut self.project_dirs.current_source,
                true,
            )
            || !info.get_string(
                "CMAKE_CURRENT_BINARY_DIR",
                &mut self.project_dirs.current_binary,
                true,
            )
        {
            return None;
        }
        self.log.raise_verbosity(verbosity);
        Some(json)
    }
}
