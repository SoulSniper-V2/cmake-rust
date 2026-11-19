use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::syntax::Location;
use crate::{Result, fail, write_changed};

#[derive(Clone, Debug)]
pub struct Tools {
    pub cc: String,
    pub cxx: String,
    pub ar: String,
}

impl Tools {
    pub fn compiler_id(compiler: &str) -> Result<&'static str> {
        let output = Command::new(compiler).arg("--version").output()?;
        if !output.status.success() {
            return fail(format!("could not query {compiler}"));
        }
        let version = String::from_utf8_lossy(&output.stdout);
        if version.contains("Apple clang") {
            Ok("AppleClang")
        } else if version.contains("clang version") {
            Ok("Clang")
        } else if version.contains("Free Software Foundation") || version.contains("GCC") {
            Ok("GNU")
        } else {
            fail(format!("unsupported compiler {compiler}: {version}"))
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Executable,
    Static,
    Shared,
    Interface,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Visibility {
    Private,
    Public,
    Interface,
}

#[derive(Clone, Debug)]
pub struct Setting {
    pub value: String,
    pub origin: Location,
    pub visibility: Visibility,
}

#[derive(Debug)]
pub struct Target {
    pub kind: Kind,
    pub source: PathBuf,
    pub binary: PathBuf,
    pub sources: Vec<String>,
    pub includes: Vec<Setting>,
    pub definitions: Vec<Setting>,
    pub options: Vec<Setting>,
    pub links: Vec<Setting>,
    pub link_options: Vec<Setting>,
    pub properties: BTreeMap<String, (String, Location)>,
    pub c_flags: Vec<String>,
    pub cxx_flags: Vec<String>,
    pub standards: [Option<String>; 2],
    pub extensions: [bool; 2],
    pub excluded: bool,
}

impl Target {
    fn property(&self, name: &str) -> Option<&str> {
        self.properties.get(name).map(|(value, _)| value.as_str())
    }
}

#[derive(Debug)]
pub struct Test {
    pub name: String,
    pub args: Vec<String>,
    pub directory: PathBuf,
    pub properties: BTreeMap<String, String>,
}

#[derive(Debug)]
pub enum Install {
    File {
        source: PathBuf,
        destination: String,
    },
    Target {
        name: String,
        destination: String,
    },
}

#[derive(Debug, Default)]
pub struct Graph {
    pub targets: BTreeMap<String, Target>,
    pub aliases: BTreeMap<String, String>,
    pub tests: Vec<Test>,
    pub install: Vec<Install>,
    pub inputs: BTreeSet<PathBuf>,
    pub directories: BTreeSet<PathBuf>,
    pub testing: bool,
    pub install_prefix: String,
}

#[derive(Debug)]
struct Artifact {
    output: PathBuf,
    linker: PathBuf,
    soname: Option<String>,
    symlinks: Vec<(PathBuf, PathBuf)>,
}

#[derive(Deserialize, Serialize)]
pub struct InstallManifest {
    pub prefix: String,
    pub items: Vec<InstallItem>,
}

#[derive(Deserialize, Serialize)]
pub struct InstallItem {
    pub source: PathBuf,
    pub destination: String,
    pub symlink: Option<PathBuf>,
}

struct Usage<'a> {
    setting: &'a Setting,
    chain: Vec<&'a str>,
}

impl Graph {
    pub fn has_target(&self, name: &str) -> bool {
        self.targets.contains_key(self.resolve(name))
    }

    fn resolve<'a>(&'a self, name: &'a str) -> &'a str {
        self.aliases.get(name).map_or(name, String::as_str)
    }

    fn artifacts(&self) -> BTreeMap<&str, Artifact> {
        self.targets
            .iter()
            .filter(|(_, target)| target.kind != Kind::Interface)
            .map(|(name, target)| (name.as_str(), artifact(name, target)))
            .collect()
    }

    fn collect_usage<'a>(
        &'a self,
        name: &'a str,
        chain: &mut Vec<&'a str>,
        visited: &mut BTreeSet<&'a str>,
        usages: &mut Vec<Usage<'a>>,
        select: fn(&Target) -> &[Setting],
    ) {
        let name = self.resolve(name);
        if !visited.insert(name) {
            return;
        }
        let Some(target) = self.targets.get(name) else {
            return;
        };
        let is_root = chain.is_empty();
        chain.push(name);
        usages.extend(
            select(target)
                .iter()
                .filter(|setting| {
                    if is_root {
                        setting.visibility != Visibility::Interface
                    } else {
                        setting.visibility != Visibility::Private
                    }
                })
                .map(|setting| Usage {
                    setting,
                    chain: chain.clone(),
                }),
        );
        for link in &target.links {
            if if is_root {
                link.visibility != Visibility::Interface
            } else {
                link.visibility != Visibility::Private
            } {
                self.collect_usage(&link.value, chain, visited, usages, select);
            }
        }
        chain.pop();
    }

    fn usage<'a>(&'a self, name: &'a str, select: fn(&Target) -> &[Setting]) -> Vec<Usage<'a>> {
        let mut usages = Vec::new();
        self.collect_usage(
            name,
            &mut Vec::new(),
            &mut BTreeSet::new(),
            &mut usages,
            select,
        );
        usages
    }

    fn link_items<'a>(
        &'a self,
        name: &'a str,
        path: &mut Vec<&'a str>,
        items: &mut Vec<&'a str>,
    ) -> Result<()> {
        if path.contains(&name) {
            return fail(format!(
                "link dependency cycle: {} -> {name}",
                path.join(" -> ")
            ));
        }
        let Some(target) = self.targets.get(name) else {
            return Ok(());
        };
        let is_root = path.is_empty();
        path.push(name);
        for link in &target.links {
            if if is_root {
                link.visibility == Visibility::Interface
            } else {
                link.visibility == Visibility::Private && target.kind != Kind::Static
            } {
                continue;
            }
            let dependency = self.resolve(&link.value);
            if let Some(other) = self.targets.get(dependency) {
                if other.kind != Kind::Interface {
                    items.push(dependency);
                }
                self.link_items(dependency, path, items)?;
            } else if dependency.contains("::") {
                return fail(format!("{}: unknown target {dependency}", link.origin));
            } else {
                items.push(dependency);
            }
        }
        path.pop();
        Ok(())
    }

    pub fn explain(&self, name: &str, needle: &str) -> Result<()> {
        if !self.has_target(name) {
            return fail(format!("unknown target {name}"));
        }
        let name = self.resolve(name);
        let mut found = false;
        for (prefix, usages) in [
            ("-I", self.usage(name, |target| &target.includes)),
            ("-D", self.usage(name, |target| &target.definitions)),
            ("", self.usage(name, |target| &target.options)),
        ] {
            for usage in usages {
                let flag = format!("{prefix}{}", usage.setting.value);
                if flag == needle || usage.setting.value == needle {
                    found = true;
                    println!(
                        "{flag} on {name}\n  {}\n  {}",
                        usage.chain.join(" -> "),
                        usage.setting.origin
                    );
                }
            }
        }
        if !found {
            return fail(format!("no tracked compile flag {needle:?} on {name}"));
        }
        Ok(())
    }

    pub fn generate(
        &self,
        build: &Path,
        tools: &Tools,
        config: &str,
        configure_command: &[String],
    ) -> Result<()> {
        let artifacts = self.artifacts();
        let mut ninja = String::from(
            "# Generated by the Rust CMake implementation\nninja_required_version = 1.7\n\n",
        );
        for (name, compiler) in [("cc", &tools.cc), ("cxx", &tools.cxx)] {
            let tool = command_arg(compiler)?;
            writeln!(
                ninja,
                "rule {name}\n  command = {tool} $flags -MMD -MF $out.d -MT $out -c $in -o $out\n  depfile = $out.d\n  deps = gcc\n  description = {name} $out"
            )?;
            writeln!(
                ninja,
                "rule link_{name}\n  command = {tool} $flags -o $out $in $libs $tail\n  description = LINK $out"
            )?;
        }
        writeln!(
            ninja,
            "rule archive\n  command = rm -f $out && {} crs $out $in\n  description = AR $out",
            command_arg(&tools.ar)?
        )?;
        writeln!(
            ninja,
            "rule symlink\n  command = ln -sf $link_name $out\n  description = SYMLINK $out"
        )?;
        let mut defaults = Vec::new();
        let mut compile_commands = Vec::new();
        for (name, target) in &self.targets {
            let Some(artifact) = artifacts.get(name.as_str()) else {
                continue;
            };
            let mut objects = Vec::new();
            let mut link_cxx = false;
            let mut include_flags = Vec::new();
            let mut definition_flags = Vec::new();
            for (values, flags, prefix) in [
                (
                    self.usage(name, |target| &target.includes),
                    &mut include_flags,
                    "-I",
                ),
                (
                    self.usage(name, |target| &target.definitions),
                    &mut definition_flags,
                    "-D",
                ),
            ] {
                for usage in values {
                    let flag = format!("{prefix}{}", usage.setting.value);
                    if !flags.contains(&flag) {
                        flags.push(flag);
                    }
                }
            }
            if let Some(symbol) = target.property("DEFINE_SYMBOL") {
                definition_flags.push(format!("-D{symbol}"));
            } else if target.kind == Kind::Shared {
                definition_flags.push(format!("-D{name}_EXPORTS"));
            }
            let options = self.usage(name, |target| &target.options);
            let mut seen_objects = BTreeSet::new();
            for file in &target.sources {
                let relative = Path::new(file);
                let extension = relative
                    .extension()
                    .and_then(|part| part.to_str())
                    .unwrap_or_default();
                let is_cxx = matches!(extension, "cc" | "cpp" | "cxx" | "C" | "c++" | "mm");
                if !is_cxx && !matches!(extension, "c" | "s" | "S" | "m") {
                    if matches!(extension, "h" | "hh" | "hpp" | "hxx" | "inc" | "inl" | "rc") {
                        continue;
                    }
                    return fail(format!(
                        "target {name}: unsupported source extension in {file:?}"
                    ));
                }
                let input = target.source.join(relative);
                let location = input
                    .strip_prefix(&target.source)
                    .ok()
                    .filter(|path| {
                        !path
                            .components()
                            .any(|part| part == std::path::Component::ParentDir)
                    })
                    .unwrap_or_else(|| Path::new(input.file_name().unwrap_or_default()));
                let object = target
                    .binary
                    .join("CMakeFiles")
                    .join(format!("{name}.dir"))
                    .join(format!("{}.o", location.display()));
                if !seen_objects.insert(object.clone()) {
                    return fail(format!(
                        "target {name}: object path collision for {}",
                        input.display()
                    ));
                }
                fs::create_dir_all(object.parent().ok_or("object parent missing")?)?;
                link_cxx |= is_cxx;
                let language = usize::from(is_cxx);
                let (rule, compiler, base_flags) = if is_cxx {
                    ("cxx", &tools.cxx, &target.cxx_flags)
                } else {
                    ("cc", &tools.cc, &target.c_flags)
                };
                let mut flags = base_flags.clone();
                if let Some(standard) = &target.standards[language] {
                    let variant = match (is_cxx, target.extensions[language]) {
                        (true, true) => "gnu++",
                        (true, false) => "c++",
                        (false, true) => "gnu",
                        (false, false) => "c",
                    };
                    flags.push(format!("-std={variant}{standard}"));
                }
                flags.extend(definition_flags.iter().cloned());
                flags.extend(include_flags.iter().cloned());
                if target.kind == Kind::Shared
                    || target
                        .property("POSITION_INDEPENDENT_CODE")
                        .is_some_and(truthy)
                {
                    flags.push("-fPIC".into());
                }
                flags.extend(options.iter().map(|entry| entry.setting.value.clone()));
                if let Some(value) = target.property("COMPILE_FLAGS") {
                    flags.extend(shell_words(value)?);
                }
                writeln!(
                    ninja,
                    "build {}: {rule} {}\n  flags = {}",
                    ninja_path(&object)?,
                    ninja_path(&input)?,
                    ninja_args(&flags)?
                )?;
                let mut command = vec![compiler.clone()];
                command.extend(flags);
                command.extend([
                    "-c".into(),
                    input.display().to_string(),
                    "-o".into(),
                    object.display().to_string(),
                ]);
                compile_commands.push(serde_json::json!({ "directory": build, "file": input, "output": object, "arguments": command }));
                objects.push(ninja_path(&object)?);
            }
            if objects.is_empty() {
                return fail(format!(
                    "cannot determine linker language for target {name}"
                ));
            }
            let output = ninja_path(&artifact.output)?;
            if target.kind == Kind::Static {
                writeln!(ninja, "build {output}: archive {}", objects.join(" "))?;
            } else {
                let mut dependencies = BTreeSet::new();
                let mut links = Vec::new();
                self.link_items(name, &mut Vec::new(), &mut links)?;
                let mut libs = Vec::new();
                let mut rpaths = BTreeSet::new();
                for link in links {
                    if let Some(dependency) = artifacts.get(link) {
                        dependencies.insert(ninja_path(&dependency.linker)?);
                        libs.push(dependency.linker.display().to_string());
                        if dependency.soname.is_some() {
                            let path = dependency
                                .output
                                .parent()
                                .ok_or("shared library parent missing")?;
                            rpaths.insert(format!("-Wl,-rpath,{}", path.display()));
                        }
                    } else if link.starts_with('-') || link.contains('/') {
                        libs.push(link.into());
                    } else {
                        libs.push(format!("-l{link}"));
                    }
                }
                let (rule, mut options) = if link_cxx {
                    ("link_cxx", target.cxx_flags.clone())
                } else {
                    ("link_cc", target.c_flags.clone())
                };
                if target.kind == Kind::Shared {
                    if cfg!(target_os = "macos") {
                        options.push("-dynamiclib".into());
                        if let Some(soname) = &artifact.soname {
                            options.push(format!("-Wl,-install_name,@rpath/{soname}"));
                        }
                        for (property, flag) in [
                            ("SOVERSION", "compatibility_version"),
                            ("VERSION", "current_version"),
                        ] {
                            if let Some(version) = target.property(property) {
                                options.push(format!("-Wl,-{flag},{version}"));
                            }
                        }
                    } else {
                        options.push("-shared".into());
                        if let Some(soname) = &artifact.soname {
                            options.push(format!("-Wl,-soname,{soname}"));
                        }
                    }
                }
                if let Some(flags) = target.property("LINK_FLAGS") {
                    options.extend(shell_words(flags)?);
                }
                options.extend(
                    self.usage(name, |target| &target.link_options)
                        .into_iter()
                        .map(|usage| usage.setting.value.clone()),
                );
                let order = if dependencies.is_empty() {
                    String::new()
                } else {
                    format!(
                        " | {}",
                        dependencies.into_iter().collect::<Vec<_>>().join(" ")
                    )
                };
                writeln!(
                    ninja,
                    "build {output}: {rule} {}{order}\n  flags = {}\n  libs = {}\n  tail = {}",
                    objects.join(" "),
                    ninja_args(&options)?,
                    ninja_args(&libs)?,
                    ninja_args(&rpaths.into_iter().collect::<Vec<_>>())?
                )?;
            }
            for (link, destination) in &artifact.symlinks {
                let filename = destination
                    .file_name()
                    .ok_or("symlink destination requires filename")?
                    .to_string_lossy();
                writeln!(
                    ninja,
                    "build {}: symlink {}\n  link_name = {}",
                    ninja_path(link)?,
                    ninja_path(destination)?,
                    command_arg(&filename)?
                )?;
            }
            let linker = ninja_path(&artifact.linker)?;
            writeln!(ninja, "build {}: phony {linker}", ninja_value(name)?)?;
            if !target.excluded {
                defaults.push(linker);
            }
        }
        let ninja_file = build.join("build.ninja");
        let inputs = self
            .inputs
            .iter()
            .filter(|path| !path.starts_with(build))
            .map(|path| ninja_path(path))
            .collect::<Result<Vec<_>>>()?;
        if !inputs.is_empty() && !configure_command.is_empty() {
            writeln!(
                ninja,
                "rule reconfigure\n  command = {}\n  generator = 1\n  restat = 1\n  description = Rerunning Rust CMake",
                ninja_args(configure_command)?
            )?;
            writeln!(ninja, "build build.ninja: reconfigure {}", inputs.join(" "))?;
            writeln!(ninja, "build {}: phony", inputs.join(" "))?;
        }
        writeln!(
            ninja,
            "build all: phony {}\ndefault all",
            defaults.join(" ")
        )?;
        self.generate_tests(config, &artifacts)?;
        let manifest = self.install_manifest(&artifacts)?;
        write_changed(
            &build.join(".cmake-rust-install.json"),
            &serde_json::to_vec_pretty(&manifest)?,
        )?;
        write_changed(
            &build.join("compile_commands.json"),
            &serde_json::to_vec_pretty(&compile_commands)?,
        )?;
        write_changed(&ninja_file, ninja.as_bytes())?;
        Ok(())
    }

    fn generate_tests(&self, config: &str, artifacts: &BTreeMap<&str, Artifact>) -> Result<()> {
        for directory in &self.directories {
            let path = directory.join("CTestTestfile.cmake");
            if !self.testing {
                if path.exists() {
                    fs::remove_file(path)?;
                }
                continue;
            }
            let mut text = String::from("# Generated by the Rust CMake implementation\n");
            for child in &self.directories {
                if child.parent() == Some(directory) {
                    writeln!(
                        text,
                        "subdirs({})",
                        cmake_quote(&child.display().to_string())
                    )?;
                }
            }
            for test in self
                .tests
                .iter()
                .filter(|test| &test.directory == directory)
            {
                let mut words = vec![cmake_quote(&test.name)];
                for (index, argument) in test.args.iter().enumerate() {
                    let arg = if index == 0 {
                        artifacts.get(self.resolve(argument)).map_or_else(
                            || argument.clone(),
                            |artifact| artifact.output.display().to_string(),
                        )
                    } else {
                        argument.clone()
                    };
                    words.push(cmake_quote(&arg.replace("$<CONFIG>", config)));
                }
                writeln!(text, "add_test({})", words.join(" "))?;
                if !test.properties.is_empty() {
                    let properties = test
                        .properties
                        .iter()
                        .flat_map(|(key, value)| [cmake_quote(key), cmake_quote(value)])
                        .collect::<Vec<_>>();
                    writeln!(
                        text,
                        "set_tests_properties({} PROPERTIES {})",
                        cmake_quote(&test.name),
                        properties.join(" ")
                    )?;
                }
            }
            write_changed(&path, text.as_bytes())?;
        }
        Ok(())
    }

    fn install_manifest(&self, artifacts: &BTreeMap<&str, Artifact>) -> Result<InstallManifest> {
        let mut manifest = InstallManifest {
            prefix: self.install_prefix.clone(),
            items: Vec::new(),
        };
        for entry in &self.install {
            match entry {
                Install::File {
                    source,
                    destination,
                } => manifest.items.push(InstallItem {
                    source: source.clone(),
                    destination: destination.clone(),
                    symlink: None,
                }),
                Install::Target { name, destination } => {
                    let artifact = artifacts
                        .get(self.resolve(name))
                        .ok_or_else(|| format!("install: unknown artifact {name}"))?;
                    manifest.items.push(InstallItem {
                        source: artifact.output.clone(),
                        destination: destination.clone(),
                        symlink: None,
                    });
                    for (link, to) in &artifact.symlinks {
                        manifest.items.push(InstallItem {
                            source: link.clone(),
                            destination: destination.clone(),
                            symlink: Some(PathBuf::from(
                                to.file_name().ok_or("missing symlink filename")?,
                            )),
                        });
                    }
                }
            }
        }
        Ok(manifest)
    }
}

fn artifact(name: &str, target: &Target) -> Artifact {
    let name = target.property("OUTPUT_NAME").unwrap_or(name);
    let version = target
        .property("VERSION")
        .or_else(|| target.property("SOVERSION"));
    let soversion = target.property("SOVERSION").or(version);
    let prefix = target
        .property("PREFIX")
        .unwrap_or(if target.kind == Kind::Executable {
            ""
        } else {
            "lib"
        });
    let suffix = target.property("SUFFIX").unwrap_or(match target.kind {
        Kind::Executable | Kind::Interface => "",
        Kind::Static => ".a",
        Kind::Shared if cfg!(target_os = "macos") => ".dylib",
        Kind::Shared => ".so",
    });
    let filename = |version: Option<&str>| match version {
        Some(version) if cfg!(target_os = "macos") => format!("{prefix}{name}.{version}{suffix}"),
        Some(version) => format!("{prefix}{name}{suffix}.{version}"),
        None => format!("{prefix}{name}{suffix}"),
    };
    let linker = target.binary.join(filename(None));
    let (output, soname, symlinks) = if target.kind == Kind::Shared {
        let output = target.binary.join(filename(version));
        let soname = filename(soversion);
        let soversion_path = target.binary.join(&soname);
        let mut symlinks = Vec::new();
        if soversion_path != output {
            symlinks.push((soversion_path.clone(), output.clone()));
        }
        if linker != soversion_path {
            symlinks.push((linker.clone(), soversion_path));
        }
        (output, Some(soname), symlinks)
    } else {
        (linker.clone(), None, Vec::new())
    };
    Artifact {
        output,
        linker,
        soname,
        symlinks,
    }
}

fn truthy(value: &str) -> bool {
    matches!(
        value.to_ascii_uppercase().as_str(),
        "1" | "ON" | "TRUE" | "YES"
    )
}

fn ninja_value(value: &str) -> Result<String> {
    if value.contains(['\r', '\n']) {
        return fail("newline in Ninja value");
    }
    Ok(value.replace('$', "$$"))
}

fn ninja_path(value: &Path) -> Result<String> {
    let value = value.to_str().ok_or("Ninja path is not UTF-8")?;
    if value.contains('|') {
        return fail("Ninja path contains '|'");
    }
    Ok(ninja_value(value)?.replace(' ', "$ ").replace(':', "$:"))
}

fn command_arg(value: &str) -> Result<String> {
    ninja_value(&shlex::try_quote(value)?)
}

fn ninja_args(args: &[String]) -> Result<String> {
    args.iter()
        .map(|argument| command_arg(argument))
        .collect::<Result<Vec<_>>>()
        .map(|parts| parts.join(" "))
}

pub fn shell_words(value: &str) -> Result<Vec<String>> {
    shlex::split(value).ok_or_else(|| format!("invalid shell quoting: {value:?}").into())
}

fn cmake_quote(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('$', "\\$")
    )
}
