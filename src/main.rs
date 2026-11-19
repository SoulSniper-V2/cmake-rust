mod cache;
mod eval;
mod graph;
mod syntax;

use std::collections::BTreeMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use cache::{Cache, CacheEntry};
use graph::{InstallManifest, Tools};
use serde::{Deserialize, Serialize};

pub(crate) type Error = Box<dyn std::error::Error>;
pub(crate) type Result<T> = std::result::Result<T, Error>;

pub(crate) fn fail<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::from(message.into()))
}

pub(crate) fn write_changed(path: &Path, data: &[u8]) -> Result<()> {
    if fs::read(path).is_ok_and(|existing| existing == data) {
        return Ok(());
    }
    let directory = path.parent().ok_or("path has no parent directory")?;
    fs::create_dir_all(directory)?;
    let name = path
        .file_name()
        .ok_or("path has no file name")?
        .to_string_lossy();
    let temporary = directory.join(format!(".{name}.tmp-{}", std::process::id()));
    fs::write(&temporary, data)?;
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    Ok(())
}

const BUILD_STATE: &str = ".cmake-rust-build.json";

#[derive(Deserialize, Serialize)]
struct BuildState {
    source: PathBuf,
    definitions: Cache,
    ninja: String,
}

fn read_build_state(build: &Path) -> Result<Option<BuildState>> {
    match fs::read(build.join(BUILD_STATE)) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

const VERSION: &str = "3.20.0";

const USAGE: &str = "\
cmake - a Rust-native implementation of the CMake command line

Usage:
  cmake -S <source> -B <build> [-G Ninja] [-D<var>[:<type>]=<value>]...
        [--fresh] [--explain <target> <flag>]
  cmake -P <script> [-D<var>=<value>]...
  cmake --build <dir> [--target <name>...] [-j [<jobs>] | --parallel [<jobs>]]
        [-- <ninja args>...]
  cmake --install <dir> [--prefix <path>]
  cmake --version
  cmake --help

Only the Ninja generator is supported. Compiler tools come from
-DCMAKE_C_COMPILER / -DCMAKE_CXX_COMPILER / -DCMAKE_AR, then the
CC / CXX / AR environment variables, then cc / c++ / ar.
--install honors the DESTDIR environment variable.
";

#[derive(Default)]
struct Invocation {
    source: Option<PathBuf>,
    positional: Option<PathBuf>,
    build: Option<PathBuf>,
    generator: Option<String>,
    definitions: Vec<String>,
    fresh: bool,
    script: Option<PathBuf>,
    explain: Option<(String, String)>,
    build_dir: Option<PathBuf>,
    targets: Vec<String>,
    jobs: Option<Option<u32>>,
    install_dir: Option<PathBuf>,
    prefix: Option<String>,
    native_args: Vec<OsString>,
    version: bool,
    help: bool,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("CMake Error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    let invocation = parse(&args)?;
    if invocation.help {
        print!("{USAGE}");
        return Ok(());
    }
    if invocation.version {
        println!(
            "cmake version {VERSION}\n\nRust-native implementation of the CMake command line."
        );
        return Ok(());
    }
    invocation.execute()
}

fn parse(args: &[OsString]) -> Result<Invocation> {
    let mut inv = Invocation::default();
    let mut index = 0;
    while index < args.len() {
        let text = args[index].to_str().ok_or("argument is not valid UTF-8")?;
        if text == "--" {
            inv.native_args.extend(args[index + 1..].iter().cloned());
            break;
        }
        if let Some(long) = text.strip_prefix("--") {
            let (name, attached) = match long.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (long, None),
            };
            match name {
                "version" => inv.version = true,
                "help" => inv.help = true,
                "fresh" => inv.fresh = true,
                "build" => {
                    inv.build_dir =
                        Some(PathBuf::from(value(args, &mut index, attached, "--build")?))
                }
                "install" => {
                    inv.install_dir = Some(PathBuf::from(value(
                        args,
                        &mut index,
                        attached,
                        "--install",
                    )?))
                }
                "prefix" => {
                    inv.prefix = Some(text_of(value(args, &mut index, attached, "--prefix")?)?)
                }
                "explain" => {
                    let target = match attached {
                        Some(target) => target.to_owned(),
                        None => text_of(value(args, &mut index, None, "--explain")?)?,
                    };
                    let flag = text_of(value(args, &mut index, None, "--explain")?)?;
                    inv.explain = Some((target, flag));
                }
                "target" => match attached {
                    Some(target) => inv.targets.push(target.to_owned()),
                    None => collect_targets(args, &mut index, &mut inv.targets, "--target")?,
                },
                "parallel" => {
                    inv.jobs = Some(parse_jobs(args, &mut index, attached, "--parallel")?)
                }
                _ => return fail(format!("unsupported option --{name}")),
            }
        } else if text.len() > 1 && text.starts_with('-') {
            let letter = text[1..].chars().next().expect("length checked above");
            let rest = &text[1 + letter.len_utf8()..];
            let attached = (!rest.is_empty()).then_some(rest);
            match letter {
                'S' => inv.source = Some(PathBuf::from(value(args, &mut index, attached, "-S")?)),
                'B' => inv.build = Some(PathBuf::from(value(args, &mut index, attached, "-B")?)),
                'G' => inv.generator = Some(text_of(value(args, &mut index, attached, "-G")?)?),
                'D' => inv
                    .definitions
                    .push(text_of(value(args, &mut index, attached, "-D")?)?),
                'P' => inv.script = Some(PathBuf::from(value(args, &mut index, attached, "-P")?)),
                't' => match attached {
                    Some(target) => inv.targets.push(target.to_owned()),
                    None => collect_targets(args, &mut index, &mut inv.targets, "-t")?,
                },
                'j' => inv.jobs = Some(parse_jobs(args, &mut index, attached, "-j")?),
                'h' => inv.help = true,
                _ => return fail(format!("unsupported option -{letter}")),
            }
        } else if inv.positional.is_none() {
            inv.positional = Some(PathBuf::from(text));
        } else {
            return fail(format!("unexpected argument {text:?}"));
        }
        index += 1;
    }
    Ok(inv)
}

fn value<'a>(
    args: &'a [OsString],
    index: &mut usize,
    attached: Option<&'a str>,
    option: &str,
) -> Result<&'a OsStr> {
    if let Some(attached) = attached {
        return Ok(OsStr::new(attached));
    }
    *index += 1;
    args.get(*index)
        .map(OsString::as_os_str)
        .ok_or_else(|| format!("{option} requires an argument").into())
}

fn text_of(value: &OsStr) -> Result<String> {
    value
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| "option value is not valid UTF-8".into())
}

fn collect_targets(
    args: &[OsString],
    index: &mut usize,
    targets: &mut Vec<String>,
    option: &str,
) -> Result<()> {
    while let Some(next) = args.get(*index + 1).and_then(|arg| arg.to_str()) {
        if next.len() > 1 && next.starts_with('-') {
            break;
        }
        targets.push(next.to_owned());
        *index += 1;
    }
    if targets.is_empty() {
        return fail(format!("{option} requires at least one target name"));
    }
    Ok(())
}

fn parse_jobs(
    args: &[OsString],
    index: &mut usize,
    attached: Option<&str>,
    option: &str,
) -> Result<Option<u32>> {
    let count = match attached {
        Some(count) => Some(count),
        None => match args.get(*index + 1).and_then(|arg| arg.to_str()) {
            Some(next) if !(next.len() > 1 && next.starts_with('-')) => {
                *index += 1;
                Some(next)
            }
            _ => None,
        },
    };
    count
        .map(|count| {
            count
                .parse::<u32>()
                .map_err(|_| -> Error { format!("invalid {option} job count {count:?}").into() })
        })
        .transpose()
}

fn cache_entries(definitions: &[String]) -> Result<Cache> {
    let mut cache = BTreeMap::new();
    for definition in definitions {
        let (name, entry) = definition.split_once('=').ok_or_else(|| {
            format!("invalid -D definition {definition:?}: expected <name>[:<type>]=<value>")
        })?;
        let (name, kind) = name.split_once(':').unwrap_or((name, "UNINITIALIZED"));
        if name.is_empty() {
            return fail(format!(
                "invalid -D definition {definition:?}: empty variable name"
            ));
        }
        cache.insert(name.to_owned(), CacheEntry::new(entry, kind, ""));
    }
    Ok(cache)
}

fn tools(cache: &mut Cache) -> Tools {
    let pick = |name: &str, environment: &str, default: &str| {
        cache
            .get(name)
            .map(|entry| entry.value.clone())
            .or_else(|| env::var(environment).ok().filter(|value| !value.is_empty()))
            .unwrap_or_else(|| default.to_owned())
    };
    let tools = Tools {
        cc: pick("CMAKE_C_COMPILER", "CC", "cc"),
        cxx: pick("CMAKE_CXX_COMPILER", "CXX", "c++"),
        ar: pick("CMAKE_AR", "AR", "ar"),
    };
    cache
        .entry("CMAKE_C_COMPILER".into())
        .or_insert_with(|| CacheEntry::new(&tools.cc, "FILEPATH", ""));
    cache
        .entry("CMAKE_CXX_COMPILER".into())
        .or_insert_with(|| CacheEntry::new(&tools.cxx, "FILEPATH", ""));
    cache
        .entry("CMAKE_AR".into())
        .or_insert_with(|| CacheEntry::new(&tools.ar, "FILEPATH", ""));
    tools
}

impl Invocation {
    fn execute(&self) -> Result<()> {
        if self.build_dir.is_some() {
            self.require(
                "--build",
                &[
                    (self.install_dir.is_some(), "--install"),
                    (self.script.is_some(), "-P"),
                    (
                        self.source.is_some() || self.positional.is_some(),
                        "a source directory",
                    ),
                    (self.build.is_some(), "-B"),
                    (self.generator.is_some(), "-G"),
                    (!self.definitions.is_empty(), "-D"),
                    (self.fresh, "--fresh"),
                    (self.explain.is_some(), "--explain"),
                    (self.prefix.is_some(), "--prefix"),
                ],
            )?;
            return self.build_tree();
        }
        if self.install_dir.is_some() {
            self.require(
                "--install",
                &[
                    (self.script.is_some(), "-P"),
                    (
                        self.source.is_some() || self.positional.is_some(),
                        "a source directory",
                    ),
                    (self.build.is_some(), "-B"),
                    (self.generator.is_some(), "-G"),
                    (!self.definitions.is_empty(), "-D"),
                    (self.fresh, "--fresh"),
                    (self.explain.is_some(), "--explain"),
                    (!self.targets.is_empty(), "--target"),
                    (self.jobs.is_some(), "--parallel/-j"),
                    (!self.native_args.is_empty(), "--"),
                ],
            )?;
            return self.install_tree();
        }
        if self.script.is_some() {
            self.require(
                "-P",
                &[
                    (
                        self.source.is_some() || self.positional.is_some(),
                        "a source directory",
                    ),
                    (self.build.is_some(), "-B"),
                    (self.generator.is_some(), "-G"),
                    (self.fresh, "--fresh"),
                    (self.explain.is_some(), "--explain"),
                    (self.prefix.is_some(), "--prefix"),
                    (!self.targets.is_empty(), "--target"),
                    (self.jobs.is_some(), "--parallel/-j"),
                    (!self.native_args.is_empty(), "--"),
                ],
            )?;
            return self.run_script();
        }
        for (present, option, required) in [
            (!self.targets.is_empty(), "--target", "--build"),
            (self.jobs.is_some(), "--parallel/-j", "--build"),
            (self.prefix.is_some(), "--prefix", "--install"),
            (!self.native_args.is_empty(), "--", "--build"),
        ] {
            if present {
                return fail(format!("{option} requires {required}"));
            }
        }
        self.configure()
    }

    fn require(&self, mode: &str, conflicts: &[(bool, &str)]) -> Result<()> {
        for (present, option) in conflicts {
            if *present {
                return fail(format!("{mode} cannot be combined with {option}"));
            }
        }
        Ok(())
    }

    fn configure(&self) -> Result<()> {
        if self.source.is_some() && self.positional.is_some() {
            return fail("source directory given both positionally and with -S");
        }
        if let Some(generator) = &self.generator {
            if generator != "Ninja" {
                return fail(format!(
                    "unsupported generator {generator:?}: only Ninja is supported"
                ));
            }
        }
        let build = self.build.clone().map_or_else(env::current_dir, Ok)?;
        fs::create_dir_all(&build)?;
        let build = build.canonicalize()?;
        let previous = read_build_state(&build)?;
        let source = self
            .source
            .clone()
            .or_else(|| self.positional.clone())
            .or_else(|| previous.as_ref().map(|state| state.source.clone()))
            .map_or_else(env::current_dir, Ok)?;
        let source = source
            .canonicalize()
            .map_err(|error| format!("source directory {}: {error}", source.display()))?;
        if !source.is_dir() {
            return fail(format!("{} is not a directory", source.display()));
        }
        if source == build {
            return fail("in-source builds are unsupported; use a separate -B directory");
        }
        if build.join("CMakeCache.txt").exists() {
            return fail("build directory contains a Kitware CMake cache; use a new -B directory");
        }
        if let Some(state) = &previous {
            if !self.fresh && state.source != source {
                return fail(format!(
                    "build directory was configured for {}",
                    state.source.display()
                ));
            }
        }
        if self.fresh && previous.is_some() {
            for name in [
                "CMakeFiles",
                "build.ninja",
                ".ninja_deps",
                ".ninja_log",
                BUILD_STATE,
                "compile_commands.json",
                ".cmake-rust-install.json",
                "CTestTestfile.cmake",
            ] {
                let path = build.join(name);
                let Ok(metadata) = path.symlink_metadata() else {
                    continue;
                };
                if metadata.is_dir() {
                    fs::remove_dir_all(&path)?;
                } else {
                    fs::remove_file(&path)?;
                }
            }
        }
        let mut cache = previous
            .filter(|_| !self.fresh)
            .map_or_else(BTreeMap::new, |state| state.definitions);
        cache.extend(cache_entries(&self.definitions)?);
        if cache.contains_key("CMAKE_TOOLCHAIN_FILE") {
            return fail("CMAKE_TOOLCHAIN_FILE and cross toolchains are unsupported");
        }
        let tools = tools(&mut cache);
        let ninja = cache
            .entry("CMAKE_MAKE_PROGRAM".into())
            .or_insert_with(|| CacheEntry::new("ninja", "FILEPATH", ""))
            .value
            .clone();
        let configure_command = self.regenerate_command(&source, &build, &cache)?;
        let mut interpreter =
            eval::Interpreter::new(source.clone(), build.clone(), cache.clone(), tools)?;
        interpreter.configure()?;
        println!("-- Configuring done");
        interpreter.graph.generate(
            &build,
            &interpreter.tools,
            &interpreter.config,
            &configure_command,
        )?;
        let state = BuildState {
            source,
            definitions: cache,
            ninja,
        };
        write_changed(
            &build.join(BUILD_STATE),
            &serde_json::to_vec_pretty(&state)?,
        )?;
        println!("-- Generating done");
        println!("-- Build files have been written to: {}", build.display());
        if let Some((target, flag)) = &self.explain {
            interpreter.graph.explain(target, flag)?;
        }
        Ok(())
    }

    fn regenerate_command(
        &self,
        source: &Path,
        build: &Path,
        definitions: &Cache,
    ) -> Result<Vec<String>> {
        let mut command = vec![
            env::current_exe()?.display().to_string(),
            "-S".to_owned(),
            source.display().to_string(),
            "-B".to_owned(),
            build.display().to_string(),
        ];
        if let Some(generator) = &self.generator {
            command.push("-G".to_owned());
            command.push(generator.clone());
        }
        for (name, entry) in definitions {
            command.push(format!("-D{name}:{}={}", entry.kind, entry.value));
        }
        Ok(command)
    }

    fn run_script(&self) -> Result<()> {
        let script = self.script.clone().expect("checked by caller");
        let cwd = env::current_dir()?;
        let mut cache = cache_entries(&self.definitions)?;
        let tools = tools(&mut cache);
        let mut interpreter = eval::Interpreter::new(cwd.clone(), cwd, cache, tools)?;
        interpreter.run_script(script)
    }

    fn build_tree(&self) -> Result<()> {
        let dir = self.build_dir.as_ref().expect("checked by caller");
        if !dir.join("build.ninja").is_file() {
            return fail(format!(
                "{} is not a configured build directory",
                dir.display()
            ));
        }
        let ninja = read_build_state(dir)?.map_or_else(|| "ninja".into(), |state| state.ninja);
        let mut command = Command::new(&ninja);
        command.current_dir(dir);
        if let Some(Some(jobs)) = self.jobs {
            command.arg("-j").arg(jobs.to_string());
        }
        command.args(&self.targets).args(&self.native_args);
        let status = command
            .status()
            .map_err(|error| format!("could not run {ninja}: {error}"))?;
        if !status.success() {
            return fail("ninja build failed");
        }
        Ok(())
    }

    fn install_tree(&self) -> Result<()> {
        let dir = self.install_dir.as_ref().expect("checked by caller");
        let manifest_path = dir.join(".cmake-rust-install.json");
        let data = fs::read(&manifest_path).map_err(|error| {
            format!(
                "{}: {error} (configure and build first)",
                manifest_path.display()
            )
        })?;
        let manifest: InstallManifest = serde_json::from_slice(&data).map_err(|error| {
            format!(
                "{}: invalid install manifest: {error}",
                manifest_path.display()
            )
        })?;
        let prefix = self.prefix.clone().unwrap_or(manifest.prefix);
        if prefix.is_empty() {
            return fail("install prefix is empty");
        }
        let destdir = env::var_os("DESTDIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        if destdir.as_ref().is_some_and(|root| !root.is_absolute()) {
            return fail("DESTDIR must be an absolute path");
        }
        if manifest.items.is_empty() {
            println!("-- Install configuration: nothing to install");
            return Ok(());
        }
        for item in &manifest.items {
            let base = if Path::new(&item.destination).is_absolute() {
                PathBuf::from(&item.destination)
            } else {
                Path::new(&prefix).join(&item.destination)
            };
            let directory = match &destdir {
                Some(root) => root.join(base.strip_prefix("/").unwrap_or(&base)),
                None => base,
            };
            fs::create_dir_all(&directory)?;
            let name = item.source.file_name().ok_or_else(|| {
                format!("install source {} has no file name", item.source.display())
            })?;
            let target = directory.join(name);
            if let Some(link) = &item.symlink {
                self.create_symlink(link, &target)?;
            } else {
                fs::copy(&item.source, &target)
                    .map_err(|error| format!("install {}: {error}", item.source.display()))?;
            }
            println!("-- Installing: {}", target.display());
        }
        Ok(())
    }

    #[cfg(unix)]
    fn create_symlink(&self, link: &Path, target: &Path) -> Result<()> {
        if target.symlink_metadata().is_ok() {
            fs::remove_file(target)?;
        }
        std::os::unix::fs::symlink(link, target)?;
        Ok(())
    }

    #[cfg(not(unix))]
    fn create_symlink(&self, _link: &Path, _target: &Path) -> Result<()> {
        fail("installing symlinks is unsupported on this platform")
    }
}
