//! Instrumentation v1 queries, timing snippets, and collection hooks.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use sha3::{Digest, Sha3_256};

use crate::interp::{CmdResult, Interp, err};
use crate::messages::MessageType;

pub const UUID: &str = "a37d1069-1972-4901-b9c9-f194aaf2b6e0";
const QUERIES: [&str; 2] = ["staticSystemInformation", "dynamicSystemInformation"];
const HOOKS: [&str; 9] = [
    "postGenerate",
    "preBuild",
    "postBuild",
    "preCMakeBuild",
    "postCMakeBuild",
    "postTest",
    "postInstall",
    "prepareForCDash",
    "manual",
];

#[derive(Debug)]
pub struct Instrumentation {
    binary: PathBuf,
    directory: PathBuf,
    pub enabled: bool,
    pub errors: bool,
    queries: BTreeSet<usize>,
    hooks: BTreeSet<usize>,
    callbacks: Vec<String>,
    cdash: Option<PathBuf>,
}

impl Instrumentation {
    pub fn new(binary: impl AsRef<Path>) -> Self {
        let binary = binary.as_ref().to_owned();
        let directory = binary.join(format!(".cmake/instrumentation-{UUID}/v1"));
        Self {
            binary,
            directory,
            enabled: false,
            errors: false,
            queries: BTreeSet::new(),
            hooks: BTreeSet::new(),
            callbacks: Vec::new(),
            cdash: None,
        }
    }

    pub fn clear_generated(&self) {
        let _ = std::fs::remove_dir_all(self.directory.join("query/generated"));
    }

    pub fn load(&mut self) {
        if self.read_queries(self.directory.join("query"))
            || self.read_queries(self.directory.join("query/generated"))
        {
            self.enabled = true;
        }
        if !self.enabled
            && let Some(config) = config_directory()
        {
            self.enabled =
                self.read_queries(config.join(format!("instrumentation-{UUID}/v1/query")));
        }
        if std::env::var("CTEST_USE_INSTRUMENTATION").is_ok_and(|x| !crate::util::is_off(&x))
            && std::env::var("CTEST_EXPERIMENTAL_INSTRUMENTATION").as_deref() == Ok(UUID)
        {
            self.enabled = true;
            self.queries.insert(1);
            self.hooks.insert(7);
            let cdash = self.directory.join("cdash");
            for subdir in ["configure", "build/commands", "build/targets", "test"] {
                let _ = std::fs::create_dir_all(cdash.join(subdir));
            }
            self.cdash = Some(cdash);
        }
    }

    fn read_queries(&mut self, path: PathBuf) -> bool {
        let mut found = false;
        for file in files(&path)
            .into_iter()
            .filter(|file| file.extension().is_some_and(|ext| ext == "json"))
        {
            found = true;
            let source = std::fs::read_to_string(&file).unwrap_or_default();
            let name = file.file_name().unwrap_or_default().to_string_lossy();
            let root: Value = match serde_json::from_str(&source) {
                Ok(root) => root,
                Err(error) => {
                    eprintln!("{name}:{}: {error}", error.line());
                    continue;
                }
            };
            match read_query(&root, &source, &name) {
                Ok((queries, hooks, callbacks)) => {
                    self.queries.extend(queries);
                    self.hooks.extend(hooks);
                    self.callbacks.extend(callbacks);
                }
                Err(error) => {
                    self.errors = true;
                    crate::cmcmd::error(format!(
                        "Could not load instrumentation queries from {}:\n{error}",
                        path.display(),
                    ));
                }
            }
        }
        found
    }

    pub fn has_hook(&self, hook: &str) -> bool {
        HOOKS
            .iter()
            .position(|name| *name == hook)
            .is_some_and(|id| self.hooks.contains(&id))
    }

    pub fn timer(&self) -> Timer {
        Timer::start(self.queries.contains(&1))
    }

    pub fn setup_launchers(&self, interp: &mut Interp) {
        let ctest = interp.cache.value("CMAKE_CTEST_COMMAND").unwrap_or("ctest");
        let launcher = if interp.is_on("CTEST_USE_LAUNCHERS") {
            format!("\"{ctest}\" --launch --current-build-dir <CMAKE_CURRENT_BINARY_DIR>")
        } else {
            format!("\"{ctest}\" --instrument")
        };
        let common = format!(
            "--target-name <TARGET_NAME> --build-dir \"{}\"",
            self.binary.display()
        );
        for (property, role, args) in [
            (
                "RULE_LAUNCH_COMPILE",
                "compile",
                "--config <CONFIG> --output <OBJECT> --source <SOURCE> --language <LANGUAGE>",
            ),
            (
                "RULE_LAUNCH_LINK",
                "link",
                "--output <TARGET> --target-type <TARGET_TYPE> --config <CONFIG> --language <LANGUAGE> --target-labels \"<TARGET_LABELS>\"",
            ),
            (
                "RULE_LAUNCH_CUSTOM",
                "custom",
                "--output \"<OUTPUT>\" --role <ROLE>",
            ),
        ] {
            interp.global_props.set(
                property,
                format!("{launcher} --command-type {role} {common} {args} -- "),
            );
        }
        interp.instrument_build_hooks = self.has_hook("preBuild") || self.has_hook("postBuild");
    }

    pub fn collect(&self, hook: &str) {
        if hook != "manual" && !self.has_hook(hook) {
            return;
        }
        let data = self.directory.join("data");
        let filename = format!("index-{}.json", suffix_time());
        let index_path = data.join(&filename);
        let _ = std::fs::create_dir_all(&data);
        if std::fs::File::create(&index_path).is_err() {
            return;
        }
        let existing = files(&data);
        let modified = |path: &Path| path.metadata().and_then(|meta| meta.modified()).ok();
        let since = existing
            .iter()
            .filter(|path| {
                **path != index_path
                    && path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .starts_with("index-")
            })
            .filter_map(|path| modified(path))
            .max();
        let snippets: Vec<_> = existing
            .iter()
            .filter(|path| {
                **path != index_path
                    && since.is_none_or(|cutoff| modified(path).is_some_and(|time| time > cutoff))
            })
            .filter_map(|path| path.file_name()?.to_str())
            .filter(|name| !name.starts_with('.'))
            .collect();
        let mut index = json!({
            "version": 1,
            "hook": hook,
            "dataDir": data,
            "buildDir": self.binary,
            "snippets": snippets,
        });
        if self.queries.contains(&0) {
            index["staticSystemInformation"] =
                crate::commands::sysinfo::instrumentation_static_info();
        }
        self.write("data", &filename, &index);
        for callback in &self.callbacks {
            let mut args = crate::commands::separate_arguments::parse_unix_command_line(callback);
            args.push(index_path.to_string_lossy().into_owned());
            if let Some((program, args)) = args.split_first() {
                let _ = Command::new(program).args(args).status();
            }
        }
        if let Some(cdash) = &self.cdash {
            for snippet in &snippets {
                Self::prepare_cdash(cdash, &data.join(snippet));
            }
        }
        for snippet in snippets {
            let _ = std::fs::remove_file(data.join(snippet));
        }
        let _ = std::fs::remove_file(index_path);
    }

    fn prepare_cdash(cdash: &Path, path: &Path) {
        let result = (|| -> Result<(), String> {
            let content = std::fs::read(path).map_err(|e| e.to_string())?;
            let value: Value = serde_json::from_slice(&content).map_err(|e| e.to_string())?;
            let role = value["role"].as_str().ok_or("Missing role")?;
            let directory = match role {
                "configure" | "generate" => cdash.join("configure"),
                "test" => cdash.join("test"),
                "compile" | "link" | "custom" | "cmakeBuild" | "cmakeInstall" | "install"
                | "ctest" => {
                    if let Some(target) = value["target"].as_str() {
                        cdash.join("build/targets").join(target)
                    } else {
                        cdash.join("build/commands")
                    }
                }
                "build" => return Ok(()),
                _ => {
                    eprintln!("Warning: Unexpected snippet type encountered: {role}");
                    return Ok(());
                }
            };
            std::fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
            let dest = directory.join(path.file_name().unwrap_or_default());
            std::fs::write(&dest, content)
                .map_err(|e| format!("Failed to copy to {}: {e}", dest.display()))
        })();
        if let Err(error) = result {
            crate::cmcmd::error(format!("{}: {error}", path.display()));
        }
    }

    pub fn write_snippet(&self, role: &str, args: &[String], clock: Timer, result: i32) {
        self.write_snippet_fields(role, args, clock, result, json!({}));
    }

    pub fn write_snippet_fields(
        &self,
        role: &str,
        args: &[String],
        clock: Timer,
        result: i32,
        fields: Value,
    ) {
        if !self.enabled {
            return;
        }
        let command = command_string(args);
        let mut snippet = json!({
            "version": 1,
            "role": role,
            "workingDir": std::env::current_dir().unwrap_or_default(),
            "result": result,
            "timeStart": clock.system,
            "duration": clock.steady.elapsed().as_millis() as u64,
        });
        if !command.is_empty() {
            snippet["command"] = json!(command);
        }
        if let Some(object) = fields.as_object() {
            for (key, value) in object {
                if !value.is_null() && value.as_str().is_none_or(|text| !text.is_empty()) {
                    snippet[key] = value.clone();
                }
            }
        }
        if self.queries.contains(&1) {
            let (before_memory, before_load) = clock.dynamic;
            let (after_memory, after_load) = dynamic_system_information();
            snippet["dynamicSystemInformation"] = json!({
                "beforeHostMemoryUsed": before_memory,
                "beforeCPULoadAverage": before_load,
                "afterHostMemoryUsed": after_memory,
                "afterCPULoadAverage": after_load,
            });
        }
        let hash = Sha3_256::digest(format!("{command}{}", std::process::id()));
        let hash: String = hash
            .iter()
            .take(10)
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let file = format!("{role}-{hash}{}.json", suffix_time());
        self.write("data", &file, &snippet);
    }

    fn write(&self, subdir: &str, filename: &str, data: &Value) {
        let directory = self.directory.join(subdir);
        // cmInstrumentation::WriteInstrumentationJson: a StreamWriterBuilder
        // with tab indentation, then "\n".
        if std::fs::create_dir_all(&directory).is_ok() {
            let mut contents = crate::commands::string::json::stream_value(data);
            contents.push('\n');
            let _ = std::fs::write(directory.join(filename), contents);
        }
    }
}

fn files(path: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(path) else {
        return Vec::new();
    };
    let mut entries: Vec<_> = entries.flatten().map(|entry| entry.path()).collect();
    entries.sort();
    entries
}

fn config_directory() -> Option<PathBuf> {
    if let Some(config) = std::env::var_os("CMAKE_CONFIG_DIR") {
        return Some(config.into());
    }
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(config.join("cmake"))
}

fn command_string(args: &[String]) -> String {
    args.iter()
        .map(|arg| format!("\"{arg}\""))
        .collect::<Vec<_>>()
        .join(" ")
}

type Query = (Vec<usize>, Vec<usize>, Vec<String>);

fn read_query(root: &Value, source: &str, filename: &str) -> Result<Query, String> {
    let context = |offset: usize, message: &str| {
        let start = source[..offset].rfind('\n').map_or(0, |pos| pos + 1);
        let line = source[start..].lines().next().unwrap_or_default();
        let line_number = source[..start]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count()
            + 1;
        format!(
            "{filename}:{line_number}: {message}\n{line}\n{}^",
            " ".repeat(offset - start)
        )
    };
    let root_at = source.find(|ch: char| !ch.is_whitespace()).unwrap_or(0);
    let object = root
        .as_object()
        .ok_or_else(|| context(root_at, "Invalid root object"))?;
    let field_at = |key| {
        source
            .find(&format!("\"{key}\""))
            .and_then(|pos| {
                let colon = source[pos..].find(':')? + pos + 1;
                Some(colon + source[colon..].find(|ch: char| !ch.is_whitespace())?)
            })
            .unwrap_or(root_at)
    };
    if !object.contains_key("version") {
        return Err(context(
            root_at,
            "Missing required field \"version\" in root object",
        ));
    }
    if root["version"]
        .as_i64()
        .is_none_or(|version| i32::try_from(version).is_err())
    {
        return Err(context(field_at("version"), "Invalid integer"));
    }
    let array = |key| -> Result<&[Value], String> {
        let Some(value) = object.get(key) else {
            return Ok(&[]);
        };
        value
            .as_array()
            .map(Vec::as_slice)
            .ok_or_else(|| context(field_at(key), "Invalid Array"))
    };
    let enums = |key, label, allowed: &[&str]| -> Result<Vec<usize>, String> {
        array(key)?
            .iter()
            .map(|value| {
                let text = value
                    .as_str()
                    .map_or_else(|| value.to_string(), str::to_owned);
                allowed
                    .iter()
                    .position(|known| *known == text)
                    .ok_or_else(|| {
                        let at = field_at(key);
                        let at = source[at..]
                            .find(&value.to_string())
                            .map_or(at, |pos| pos + at);
                        context(at, &format!("Not a valid {label}: \"{text}\""))
                    })
            })
            .collect()
    };
    let queries = enums("queries", "query", &QUERIES)?;
    let hooks = enums("hooks", "hook", &HOOKS)?;
    let callbacks = array("callbacks")?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| context(field_at("callbacks"), "Invalid string"))
        })
        .collect::<Result<_, _>>()?;
    if let Some(key) = object
        .keys()
        .find(|key| !matches!(key.as_str(), "version" | "queries" | "hooks" | "callbacks"))
    {
        return Err(context(
            field_at(key),
            &format!("Invalid extra field \"{key}\" in root object"),
        ));
    }
    Ok((queries, hooks, callbacks))
}

pub struct Timer {
    steady: Instant,
    system: u64,
    dynamic: (f64, f64),
}

impl Timer {
    pub fn start(measure: bool) -> Self {
        Self {
            steady: Instant::now(),
            system: epoch_millis(),
            dynamic: if measure {
                dynamic_system_information()
            } else {
                (0.0, 0.0)
            },
        }
    }
}

fn epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn suffix_time() -> String {
    let millis = epoch_millis();
    let seconds = (millis / 1000) as libc::time_t;
    // SAFETY: both pointers are valid and gmtime_r initializes tm on success.
    let date = unsafe {
        let mut out = std::mem::MaybeUninit::<libc::tm>::uninit();
        if libc::gmtime_r(&seconds, out.as_mut_ptr()).is_null() {
            return millis.to_string();
        }
        out.assume_init()
    };
    format!(
        "{:04}-{:02}-{:02}T{:02}-{:02}-{:02}-{:04}",
        date.tm_year + 1900,
        date.tm_mon + 1,
        date.tm_mday,
        date.tm_hour,
        date.tm_min,
        date.tm_sec,
        millis % 1000
    )
}

fn dynamic_system_information() -> (f64, f64) {
    let mut total = 0u64;
    let mut available = None;
    let mut free_and_cached = 0;
    let memory = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    for (key, value) in memory.lines().filter_map(|line| line.split_once(':')) {
        let number = value
            .split_whitespace()
            .next()
            .and_then(|v| v.parse().ok())
            .unwrap_or_default();
        match key {
            "MemTotal" => total = number,
            "MemAvailable" => available = Some(number),
            "MemFree" | "Buffers" | "Cached" => free_and_cached += number,
            _ => {}
        }
    }
    let mut load = 0.0;
    // SAFETY: getloadavg receives space for one f64 and at most one value.
    unsafe { libc::getloadavg(&mut load, 1) };
    (
        total.saturating_sub(available.unwrap_or(free_and_cached)) as f64,
        load,
    )
}

pub fn register(interp: &mut Interp) {
    interp.register("cmake_instrumentation", cmake_instrumentation);
}

fn cmake_instrumentation(interp: &mut Interp, args: &[String]) -> CmdResult {
    if crate::commands::language::experimental_enabled(interp, "Instrumentation") != Some(true) {
        return err("requires the experimental Instrumentation flag to be enabled");
    }
    if args.is_empty() {
        return err("must be called with arguments.");
    }
    let keywords = [
        "API_VERSION",
        "DATA_VERSION",
        "QUERIES",
        "HOOKS",
        "CALLBACK",
    ];
    let mut versions = ["", ""];
    let mut queries = Vec::new();
    let mut hooks = Vec::new();
    let mut callbacks = Vec::new();
    let mut missing = Vec::new();
    let mut unknown = None;
    let mut index = 0;
    while index < args.len() {
        let keyword = &args[index];
        index += 1;
        let Some(id) = keywords.iter().position(|word| word == keyword) else {
            unknown.get_or_insert(keyword);
            continue;
        };
        let start = index;
        while index < args.len() && !keywords.contains(&args[index].as_str()) {
            index += 1;
            if id < 2 {
                break;
            }
        }
        if start == index {
            missing.push(keyword);
            continue;
        }
        let values = &args[start..index];
        match id {
            0 | 1 => versions[id] = &values[0],
            2 => queries.extend_from_slice(values),
            3 => hooks.extend_from_slice(values),
            _ => callbacks.push(command_string(values)),
        }
    }
    if !missing.is_empty() {
        let errors: String = missing
            .iter()
            .map(|key| format!("  Error after keyword \"{key}\":\n    missing required value\n"))
            .collect();
        interp.issue(
            MessageType::FatalError,
            &format!("  called with the following argument errors:\n{errors}"),
        );
        return Ok(());
    }
    if let Some(unknown) = unknown {
        return err(format!("given unknown argument \"{unknown}\"."));
    }
    for (key, version) in keywords.into_iter().zip(versions) {
        if !version.bytes().all(|byte| byte.is_ascii_digit()) {
            return err(format!("given a non-integer {key}."));
        }
        if version.trim_start_matches('0') != "1" {
            return err(format!(
                "QUERY subcommand given an unsupported {key} \"{version}\" (the only currently supported version is 1)."
            ));
        }
    }
    let validate = |values: &[String], key, allowed: &[&str]| {
        values
            .iter()
            .map(|value| {
                allowed
                    .iter()
                    .position(|option| option == value)
                    .ok_or_else(|| format!("given invalid argument to {key} \"{value}\""))
            })
            .collect::<Result<BTreeSet<usize>, _>>()
    };
    let queries = match validate(&queries, "QUERIES", &QUERIES) {
        Ok(queries) => queries,
        Err(error) => return err(error),
    };
    let hooks = match validate(&hooks, "HOOKS", &HOOKS) {
        Ok(hooks) => hooks,
        Err(error) => return err(error),
    };
    let instrumentation = Instrumentation::new(&interp.home_binary);
    let id = files(&instrumentation.directory.join("query/generated")).len();
    instrumentation.write(
        "query/generated",
        &format!("query-{id}.json"),
        &json!({
            "version": 1,
            "queries": queries.into_iter().map(|id| QUERIES[id]).collect::<Vec<_>>(),
            "hooks": hooks.into_iter().map(|id| HOOKS[id]).collect::<Vec<_>>(),
            "callbacks": callbacks,
        }),
    );
    Ok(())
}
