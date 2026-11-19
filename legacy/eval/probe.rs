use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::{Interpreter, need};
use crate::graph::shell_words;
use crate::{Result, fail};

const KEYWORDS: &[&str] = &[
    "CMAKE_FLAGS",
    "COMPILE_DEFINITIONS",
    "LINK_OPTIONS",
    "LINK_LIBRARIES",
    "OUTPUT_VARIABLE",
    "COPY_FILE",
    "COPY_FILE_ERROR",
    "NO_CACHE",
    "NO_LOG",
    "C_STANDARD",
    "CXX_STANDARD",
    "SOURCES",
    "SOURCE_FROM_CONTENT",
    "PROJECT",
];

impl Interpreter {
    pub(super) fn try_compile_command(&mut self, args: &[String]) -> Result<()> {
        need(args, 3)?;
        let result_name = &args[0];
        let mut index = 2;
        let binary = if KEYWORDS.contains(&args[1].as_str()) {
            index = 1;
            self.directory().binary.clone()
        } else {
            self.directory().binary.join(&args[1])
        };
        let temporary = ProbeDirectory::new(&binary)?;
        let mut sources = Vec::new();
        let mut definitions = Vec::new();
        let mut link_options = Vec::new();
        let mut libraries = Vec::new();
        let mut output_variable = None;
        let mut copy_file = None;
        let mut copy_error = None;
        let mut cache = true;
        let mut standards = [self.get("CMAKE_C_STANDARD"), self.get("CMAKE_CXX_STANDARD")];
        if index == 2 && !KEYWORDS.contains(&args[index].as_str()) {
            sources.push(self.directory().source.join(&args[index]));
            index += 1;
        }
        while let Some(keyword) = args.get(index) {
            index += 1;
            let tail = &args[index..];
            let length = tail
                .iter()
                .position(|value| KEYWORDS.contains(&value.as_str()))
                .unwrap_or(tail.len());
            let values = &tail[..length];
            index += length;
            match (keyword.as_str(), values) {
                ("SOURCES", paths) => {
                    sources.extend(paths.iter().map(|path| self.directory().source.join(path)))
                }
                ("SOURCE_FROM_CONTENT", [name, content]) => {
                    let path = Path::new(name);
                    if path.file_name().and_then(|part| part.to_str()) != Some(name.as_str()) {
                        return fail(format!("invalid try_compile source name {name}"));
                    }
                    let path = temporary.0.join(path);
                    fs::write(&path, content)?;
                    sources.push(path);
                }
                ("COMPILE_DEFINITIONS", options) => definitions.extend_from_slice(options),
                ("LINK_OPTIONS", options) => link_options.extend_from_slice(options),
                ("LINK_LIBRARIES", links) => libraries.extend_from_slice(links),
                ("NO_CACHE", []) => cache = false,
                ("NO_LOG", []) => {}
                ("OUTPUT_VARIABLE", [name]) => output_variable = Some(name),
                ("COPY_FILE", [path]) => copy_file = Some(path),
                ("COPY_FILE_ERROR", [name]) => copy_error = Some(name),
                ("C_STANDARD", [value]) => standards[0] = Some(value.clone()),
                ("CXX_STANDARD", [value]) => standards[1] = Some(value.clone()),
                ("CMAKE_FLAGS", flags) => {
                    for flag in flags {
                        let (key, value) = flag
                            .strip_prefix("-D")
                            .and_then(|flag| flag.split_once('='))
                            .ok_or_else(|| {
                                format!("unsupported try_compile CMAKE_FLAGS argument {flag:?}")
                            })?;
                        match key.split(':').next().unwrap_or(key) {
                            "CMAKE_C_STANDARD" => standards[0] = Some(value.into()),
                            "CMAKE_CXX_STANDARD" => standards[1] = Some(value.into()),
                            _ => {
                                return fail(format!(
                                    "unsupported try_compile CMAKE_FLAGS definition {flag:?}"
                                ));
                            }
                        }
                    }
                }
                _ => return fail(format!("unsupported try_compile argument {keyword}")),
            }
        }
        if sources.is_empty() {
            return fail("try_compile needs source files");
        }
        let mut log = String::new();
        let mut objects = Vec::new();
        let mut uses_cxx = false;
        let mut success = true;
        for (index, source) in sources.iter().enumerate() {
            let is_cxx = match source.extension().and_then(|part| part.to_str()) {
                Some("cc" | "cpp" | "cxx" | "C" | "c++") => true,
                Some("c" | "s" | "S") => false,
                _ => {
                    return fail(format!(
                        "unsupported try_compile source {}",
                        source.display()
                    ));
                }
            };
            if !source.is_file() {
                return fail(format!(
                    "try_compile source does not exist: {}",
                    source.display()
                ));
            }
            self.graph.inputs.insert(source.clone());
            uses_cxx |= is_cxx;
            let language = if is_cxx { "CXX" } else { "C" };
            let tool = if is_cxx {
                &self.tools.cxx
            } else {
                &self.tools.cc
            };
            let object = temporary.0.join(format!("{index}.o"));
            let mut compiler = Command::new(tool);
            compiler
                .args(self.probe_flags(language)?)
                .args(&definitions);
            if let Some(standard) = &standards[usize::from(is_cxx)] {
                let dialect = if is_cxx { "c++" } else { "c" };
                compiler.arg(format!("-std={dialect}{standard}"));
            }
            compiler.arg("-c").arg(source).arg("-o").arg(&object);
            if !run_probe(&mut compiler, &mut log)? {
                success = false;
                break;
            }
            objects.push(object);
        }
        let kind = self
            .get("CMAKE_TRY_COMPILE_TARGET_TYPE")
            .unwrap_or_else(|| "EXECUTABLE".into());
        let executable = temporary.0.join(if kind == "STATIC_LIBRARY" {
            "libprobe.a"
        } else {
            "probe"
        });
        if success {
            let mut link = match kind.as_str() {
                "STATIC_LIBRARY" => {
                    if !link_options.is_empty() || !libraries.is_empty() {
                        return fail(
                            "try_compile STATIC_LIBRARY with link options or libraries is unsupported",
                        );
                    }
                    let mut command = Command::new(&self.tools.ar);
                    command.arg("crs").arg(&executable).args(&objects);
                    command
                }
                "EXECUTABLE" => {
                    let (tool, language) = if uses_cxx {
                        (&self.tools.cxx, "CXX")
                    } else {
                        (&self.tools.cc, "C")
                    };
                    let mut command = Command::new(tool);
                    command
                        .args(self.probe_flags(language)?)
                        .arg("-o")
                        .arg(&executable)
                        .args(&objects)
                        .args(&link_options);
                    for library in &libraries {
                        if library.contains("::") {
                            return fail(format!(
                                "try_compile imported target {library} is unsupported"
                            ));
                        }
                        if library.starts_with('-') || library.contains('/') {
                            command.arg(library);
                        } else {
                            command.arg(format!("-l{library}"));
                        }
                    }
                    command
                }
                _ => return fail(format!("unknown try_compile target type {kind}")),
            };
            success = run_probe(&mut link, &mut log)?;
        }
        if let Some(name) = output_variable {
            self.set(name, log);
        }
        if let Some(destination) = copy_file.filter(|_| success) {
            let destination = self.directory().binary.join(destination);
            match fs::copy(&executable, destination) {
                Ok(_) => {
                    if let Some(name) = copy_error {
                        self.set(name, "");
                    }
                }
                Err(error) => {
                    if let Some(name) = copy_error {
                        self.set(name, error.to_string());
                    } else {
                        return Err(error.into());
                    }
                }
            }
        }
        let value = if success { "TRUE" } else { "FALSE" };
        if cache {
            self.cache_set(result_name, value, "INTERNAL", "Result of TRY_COMPILE");
        } else {
            self.set(result_name, value);
        }
        Ok(())
    }

    fn probe_flags(&self, language: &str) -> Result<Vec<String>> {
        let mut flags = shell_words(
            &self
                .get(&format!("CMAKE_{language}_FLAGS"))
                .unwrap_or_default(),
        )?;
        if let Some(config) = self
            .get("CMAKE_TRY_COMPILE_CONFIGURATION")
            .or_else(|| self.get("CMAKE_BUILD_TYPE"))
        {
            let config_flags = self
                .get(&format!(
                    "CMAKE_{language}_FLAGS_{}",
                    config.to_ascii_uppercase()
                ))
                .unwrap_or_default();
            flags.extend(shell_words(&config_flags)?);
        }
        Ok(flags)
    }
}

struct ProbeDirectory(PathBuf);

impl ProbeDirectory {
    fn new(binary: &Path) -> Result<Self> {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = binary.join("CMakeFiles/CMakeScratch");
        fs::create_dir_all(&root)?;
        loop {
            let path = root.join(format!(
                "TryCompile-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
    }
}

impl Drop for ProbeDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run_probe(command: &mut Command, log: &mut String) -> Result<bool> {
    use std::fmt::Write as _;
    writeln!(log, "{command:?}")?;
    let output = command.output()?;
    log.push_str(&String::from_utf8_lossy(&output.stdout));
    log.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok(output.status.success())
}
