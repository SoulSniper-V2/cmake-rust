//! File and module handling for `include()`, plus native implementations of
//! the check modules that projects normally include from CMake's own module
//! directory (CheckFunctionExists, CheckTypeSize, and friends). The check
//! commands run real compiler and linker probes with the detected toolchain,
//! honoring the `CMAKE_REQUIRED_*` variables, and store their results in the
//! interpreter cache the same way CMake's try_compile-based checks do.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::graph::shell_words;
use crate::syntax::{Argument, Form, split_list};
use crate::{Result, fail};

use super::condition::cmake_regex;
use super::{Interpreter, is_false, need, path_string};

/// CMake built-in modules implemented natively. Keys are lowercase module
/// names as passed to `include()`, stored in `self.native_modules`.
const NATIVE_MODULES: &[&str] = &[
    "checktypesize",
    "checkfunctionexists",
    "checkincludefile",
    "checkincludefiles",
    "checkincludefilecxx",
    "checkcsourcecompiles",
    "checkcxxsourcecompiles",
    "checksymbolexists",
    "checkcxxsymbolexists",
    "checkstructhasmember",
    "checklibraryexists",
    "checkccompilerflag",
    "checkcxxcompilerflag",
    "checklinkerflag",
    "cmakedependentoption",
    "cmakepushcheckstate",
    "gnuinstalldirs",
];

/// Maps a command name to the native module that provides it, mirroring how
/// CMake's own modules define these macros on inclusion.
fn command_module(command: &str) -> Option<&'static str> {
    Some(match command {
        "check_type_size" => "checktypesize",
        "check_function_exists" => "checkfunctionexists",
        "check_include_file" => "checkincludefile",
        "check_include_files" => "checkincludefiles",
        "check_include_file_cxx" => "checkincludefilecxx",
        "check_c_source_compiles" => "checkcsourcecompiles",
        "check_cxx_source_compiles" => "checkcxxsourcecompiles",
        "check_symbol_exists" => "checksymbolexists",
        "check_cxx_symbol_exists" => "checkcxxsymbolexists",
        "check_struct_has_member" => "checkstructhasmember",
        "check_library_exists" => "checklibraryexists",
        "check_c_compiler_flag" => "checkccompilerflag",
        "check_cxx_compiler_flag" => "checkcxxcompilerflag",
        "check_linker_flag" => "checklinkerflag",
        "cmake_dependent_option" => "cmakedependentoption",
        "cmake_push_check_state" | "cmake_pop_check_state" | "cmake_reset_check_state" => {
            "cmakepushcheckstate"
        }
        _ => return None,
    })
}

/// Variables saved and restored by CMakePushCheckState.
const CHECK_STATE_VARIABLES: &[&str] = &[
    "CMAKE_REQUIRED_FLAGS",
    "CMAKE_REQUIRED_DEFINITIONS",
    "CMAKE_REQUIRED_INCLUDES",
    "CMAKE_REQUIRED_LIBRARIES",
    "CMAKE_REQUIRED_LINK_OPTIONS",
    "CMAKE_REQUIRED_LINK_DIRECTORIES",
    "CMAKE_REQUIRED_QUIET",
    "CMAKE_EXTRA_INCLUDE_FILES",
];

/// Compiler/linker output that makes a flag check fail even when the
/// invocation exits successfully (warnings about unknown or ignored flags).
/// Mirrors the FAIL_REGEX list in CMake's CheckCompilerFlag.cmake.
const FLAG_FAIL_PATTERNS: &[&str] = &[
    "[Uu]nrecogni[sz]ed .*[Oo]ption",
    "[Uu]nknown (warning )?[Oo]ption",
    "[Oo]ption.*not supported",
    "optimization flag.*not supported",
    "argument unused during compilation",
    "[Ii]gnoring (unknown|invalid) option",
    "[Ww]arning: [Oo]ption",
    "invalid argument .*option",
    "is valid for .* but not for",
    "option will be ignored",
];

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Language {
    #[default]
    C,
    Cxx,
}

/// Parameters for one compiler or linker probe.
#[derive(Default)]
struct Probe<'a> {
    language: Language,
    /// Link an executable rather than compiling an object only.
    link: bool,
    source: &'a str,
    extra_flags: &'a [String],
    extra_link_items: &'a [String],
    /// Output matching any of these patterns fails the probe.
    fail_regexes: &'a [String],
}

impl Interpreter {
    /// `include(<file|module> [OPTIONAL] [RESULT_VARIABLE <var>] [NO_POLICY_SCOPE])`.
    /// `NO_POLICY_SCOPE` is accepted and ignored since policies are not
    /// implemented.
    pub(super) fn include_command(&mut self, args: &[String]) -> Result<()> {
        need(args, 1)?;
        let name = args[0].clone();
        if name.is_empty() {
            return fail("include called with an empty file name");
        }
        let mut optional = false;
        let mut no_policy_scope = false;
        let mut result_variable = None;
        let mut index = 1;
        while index < args.len() {
            match args[index].as_str() {
                "OPTIONAL" => optional = true,
                "NO_POLICY_SCOPE" => no_policy_scope = true,
                "RESULT_VARIABLE" => {
                    index += 1;
                    result_variable = Some(
                        args.get(index)
                            .ok_or("include: RESULT_VARIABLE requires a variable name")?
                            .clone(),
                    );
                }
                other => return fail(format!("include: unexpected argument {other:?}")),
            }
            index += 1;
        }
        let mut result = "NOTFOUND".to_owned();
        if let Some(path) = self.find_include(&name) {
            self.run_include(path.clone(), no_policy_scope)?;
            result =
                fs::canonicalize(&path).map_or_else(|_| path_string(&path), |p| path_string(&p));
        } else if is_module_name(&name)
            && NATIVE_MODULES.contains(&name.to_ascii_lowercase().as_str())
        {
            let module = name.to_ascii_lowercase();
            self.native_modules.insert(module.clone());
            self.native_module_init(&module)?;
            result = name;
        } else if let Some(path) = self.find_distribution_module(&name) {
            self.run_include(path.clone(), no_policy_scope)?;
            result = path_string(&path.canonicalize()?);
        } else if !optional {
            return fail(format!(
                "include could not find requested file:\n\n  {name}"
            ));
        }
        if let Some(variable) = result_variable {
            self.set(&variable, result);
        }
        Ok(())
    }

    pub(super) fn find_package(&mut self, args: &[String]) -> Result<()> {
        need(args, 1)?;
        let name = &args[0];
        if !is_module_name(name) {
            return fail(format!("invalid package name {name:?}"));
        }

        let mut tail = &args[1..];
        let mut version = None;
        if let Some(first) = tail
            .first()
            .filter(|arg| arg.starts_with(|ch: char| ch.is_ascii_digit()))
        {
            if !first
                .split('.')
                .all(|part| !part.is_empty() && part.bytes().all(|ch| ch.is_ascii_digit()))
            {
                return fail(format!("unsupported find_package version {first:?}"));
            }
            version = Some(first.as_str());
            tail = &tail[1..];
        }
        let mut exact = false;
        let mut quiet = false;
        let mut required = false;
        let mut module_only = false;
        let mut components = Vec::new();
        let mut component_required = None;
        for arg in tail {
            match arg.as_str() {
                "EXACT" if version.is_some() => exact = true,
                "QUIET" => quiet = true,
                "REQUIRED" => required = true,
                "MODULE" => module_only = true,
                "COMPONENTS" => component_required = Some(true),
                "OPTIONAL_COMPONENTS" => component_required = Some(false),
                "CONFIG" | "NO_MODULE" | "NAMES" | "CONFIGS" | "HINTS" | "PATHS"
                | "NO_DEFAULT_PATH" | "GLOBAL" | "BYPASS_PROVIDER" | "UNWIND_INCLUDE" => {
                    return fail(format!("unsupported find_package option {arg}"));
                }
                _ => match component_required {
                    Some(required) => components.push((arg.as_str(), required)),
                    None => return fail(format!("unsupported find_package argument {arg:?}")),
                },
            }
        }
        let module = format!("Find{name}");
        let Some(file) = self
            .find_include(&module)
            .or_else(|| self.find_distribution_module(&module))
        else {
            return if module_only && !required {
                self.set(format!("{name}_FOUND"), "0");
                Ok(())
            } else {
                fail(format!(
                    "no {module}.cmake found; package config search is unsupported"
                ))
            };
        };

        let prefix = format!("{name}_FIND_");
        let mut context = BTreeMap::new();
        for (key, value) in [
            ("QUIETLY", quiet),
            ("REQUIRED", required),
            ("VERSION_EXACT", exact),
        ] {
            context.insert(
                format!("{prefix}{key}"),
                String::from(if value { "1" } else { "0" }),
            );
        }
        context.insert(
            format!("{prefix}COMPONENTS"),
            components
                .iter()
                .map(|(name, _)| *name)
                .collect::<Vec<_>>()
                .join(";"),
        );
        for (component, required) in components {
            context.insert(
                format!("{prefix}REQUIRED_{component}"),
                String::from(if required { "1" } else { "0" }),
            );
        }
        if let Some(version) = version {
            context.insert(format!("{prefix}VERSION"), version.into());
            context.insert(
                format!("{prefix}VERSION_COUNT"),
                version.split('.').count().to_string(),
            );
            for (part, value) in ["MAJOR", "MINOR", "PATCH", "TWEAK"]
                .into_iter()
                .zip(version.split('.').chain(std::iter::repeat("0")))
            {
                context.insert(format!("{prefix}VERSION_{part}"), value.into());
            }
        }
        context.insert("CMAKE_FIND_PACKAGE_NAME".into(), name.clone());

        let mut original = Vec::new();
        for (key, value) in context {
            let previous = self.variables_mut().insert(key.clone(), value);
            original.push((key, previous));
        }
        let result = self.run_include(file, false);
        for (key, value) in original {
            if let Some(value) = value {
                self.set(key, value);
            } else {
                self.variables_mut().remove(&key);
            }
        }
        result?;
        if required
            && [
                format!("{name}_FOUND"),
                format!("{}_FOUND", name.to_ascii_uppercase()),
            ]
            .iter()
            .all(|var| self.get(var).is_none_or(|value| is_false(&value)))
        {
            return fail(format!("required package {name} was not found"));
        }
        Ok(())
    }

    // A distribution's .cmake files can be interpreted directly when
    // CMAKE_ROOT is provided. This never looks for a CMake executable.
    fn find_distribution_module(&self, name: &str) -> Option<PathBuf> {
        if !is_module_name(name) {
            return None;
        }
        let root = PathBuf::from(self.get("CMAKE_ROOT")?);
        let module = root.join("Modules").join(format!("{name}.cmake"));
        module.is_file().then_some(module)
    }

    /// Whether `name` is a native command provided by an included module.
    pub(super) fn knows_native_command(&self, name: &str) -> bool {
        command_module(name).is_some_and(|module| self.native_modules.contains(module))
    }

    /// Runs a native module command. Returns `Ok(false)` when `name` is not
    /// provided by any included native module so the caller can handle it as
    /// an unknown command.
    pub(super) fn native_command(&mut self, name: &str, args: &[String]) -> Result<bool> {
        if !self.knows_native_command(name) {
            return Ok(false);
        }
        match name {
            "check_type_size" => self.check_type_size(args)?,
            "check_function_exists" => self.check_function_exists(args)?,
            "check_include_file" => self.check_include_file(args, Language::C)?,
            "check_include_files" => self.check_include_files(args, Language::C)?,
            "check_include_file_cxx" => self.check_include_file(args, Language::Cxx)?,
            "check_c_source_compiles" => self.check_source_compiles(args, Language::C)?,
            "check_cxx_source_compiles" => self.check_source_compiles(args, Language::Cxx)?,
            "check_symbol_exists" => self.check_symbol_exists(args, Language::C)?,
            "check_cxx_symbol_exists" => self.check_symbol_exists(args, Language::Cxx)?,
            "check_struct_has_member" => self.check_struct_has_member(args)?,
            "check_library_exists" => self.check_library_exists(args)?,
            "check_c_compiler_flag" => self.check_compiler_flag(args, Language::C)?,
            "check_cxx_compiler_flag" => self.check_compiler_flag(args, Language::Cxx)?,
            "check_linker_flag" => self.check_linker_flag(args)?,
            "cmake_dependent_option" => self.cmake_dependent_option(args)?,
            "cmake_push_check_state" => self.cmake_push_check_state(args)?,
            "cmake_pop_check_state" => self.cmake_pop_check_state(args)?,
            "cmake_reset_check_state" => self.cmake_reset_check_state(args)?,
            _ => unreachable!("command_module() covers every dispatched command"),
        }
        Ok(true)
    }

    /// Runs include-time side effects for native modules that set variables
    /// when included rather than only defining commands.
    fn native_module_init(&mut self, module: &str) -> Result<()> {
        match module {
            "gnuinstalldirs" => self.gnu_install_dirs(),
            _ => Ok(()),
        }
    }

    /// Locates `name` for `include()`. Module names are searched in
    /// `CMAKE_MODULE_PATH`, then the current list directory and the current
    /// source directory; paths and `*.cmake` names are resolved relative to
    /// the current source directory, falling back to the current list
    /// directory.
    fn find_include(&self, name: &str) -> Option<PathBuf> {
        let path = Path::new(name);
        if path.is_absolute() {
            return path.is_file().then(|| path.to_path_buf());
        }
        let mut candidates = Vec::new();
        if is_module_name(name) {
            let file = format!("{name}.cmake");
            if let Some(module_path) = self.get("CMAKE_MODULE_PATH") {
                for entry in split_list(&module_path) {
                    if entry.is_empty() {
                        continue;
                    }
                    let dir = absolute(&entry, &self.directory().source);
                    candidates.push(dir.join(&file));
                }
            }
            candidates.push(self.list_dir().join(&file));
            candidates.push(self.directory().source.join(&file));
        } else {
            for base in [self.directory().source.clone(), self.list_dir()] {
                candidates.push(base.join(name));
                if !name.ends_with(".cmake") {
                    candidates.push(base.join(format!("{name}.cmake")));
                }
            }
        }
        candidates.into_iter().find(|candidate| candidate.is_file())
    }

    /// Directory of the list file currently being evaluated.
    fn list_dir(&self) -> PathBuf {
        self.files
            .last()
            .and_then(|file| file.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| self.directory().source.clone())
    }

    /// Whether check modules should suppress their status messages.
    fn checks_quiet(&self) -> bool {
        self.get("CMAKE_REQUIRED_QUIET")
            .is_some_and(|value| !is_false(&value))
    }

    /// Compile flags honoring `CMAKE_REQUIRED_FLAGS`,
    /// `CMAKE_REQUIRED_DEFINITIONS`, and `CMAKE_REQUIRED_INCLUDES`.
    fn required_flags(&self) -> Result<Vec<String>> {
        let mut flags = Vec::new();
        if let Some(value) = self.get("CMAKE_REQUIRED_FLAGS") {
            flags.extend(shell_words(&value)?);
        }
        for (variable, prefix) in [
            ("CMAKE_REQUIRED_DEFINITIONS", "-D"),
            ("CMAKE_REQUIRED_INCLUDES", "-I"),
        ] {
            if let Some(value) = self.get(variable) {
                for item in split_list(&value) {
                    if item.is_empty() {
                        continue;
                    }
                    if variable == "CMAKE_REQUIRED_DEFINITIONS" && item.starts_with('-') {
                        flags.push(item);
                    } else {
                        flags.push(format!("{prefix}{item}"));
                    }
                }
            }
        }
        Ok(flags)
    }

    /// Link items honoring `CMAKE_REQUIRED_LINK_DIRECTORIES`,
    /// `CMAKE_REQUIRED_LINK_OPTIONS`, and `CMAKE_REQUIRED_LIBRARIES`.
    fn required_link_items(&self) -> Vec<String> {
        let mut items = Vec::new();
        if let Some(value) = self.get("CMAKE_REQUIRED_LINK_DIRECTORIES") {
            for item in split_list(&value) {
                if !item.is_empty() {
                    items.push(format!("-L{item}"));
                }
            }
        }
        if let Some(value) = self.get("CMAKE_REQUIRED_LINK_OPTIONS") {
            items.extend(
                split_list(&value)
                    .into_iter()
                    .filter(|item| !item.is_empty()),
            );
        }
        if let Some(value) = self.get("CMAKE_REQUIRED_LIBRARIES") {
            for item in split_list(&value) {
                if item.is_empty() {
                    continue;
                }
                items.push(if item.starts_with('-') || item.contains('/') {
                    item
                } else {
                    format!("-l{item}")
                });
            }
        }
        items
    }

    /// Runs a real compiler or linker probe without a shell, like
    /// `try_compile`. Returns whether it succeeded and the combined
    /// compiler/linker output.
    fn try_compile(&self, probe: &Probe<'_>) -> Result<(bool, String)> {
        let scratch = self
            .directory()
            .binary
            .join("CMakeFiles")
            .join("CMakeScratch");
        fs::create_dir_all(&scratch)?;
        let cxx = probe.language == Language::Cxx;
        let tag = if cxx { "cxx" } else { "cc" };
        let extension = if cxx { "cpp" } else { "c" };
        let input = scratch.join(format!("probe_{tag}.{extension}"));
        fs::write(&input, probe.source)?;
        let compiler = if cxx { &self.tools.cxx } else { &self.tools.cc };
        let mut command = Command::new(compiler.as_str());
        command.args(self.required_flags()?).args(probe.extra_flags);
        if probe.link {
            command
                .arg(&input)
                .arg("-o")
                .arg(scratch.join(format!("probe_{tag}")))
                .args(self.required_link_items())
                .args(probe.extra_link_items);
        } else {
            command
                .arg("-c")
                .arg(&input)
                .arg("-o")
                .arg(scratch.join(format!("probe_{tag}.o")));
        }
        let output = command
            .output()
            .map_err(|error| format!("could not run {compiler}: {error}"))?;
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        Ok((output.status.success(), text))
    }

    /// Shared body for the cached check commands: skips the probe when
    /// `variable` is already defined, runs it otherwise, reports like CMake's
    /// `message(STATUS)`, and stores "1" or "" in the cache.
    fn perform_check(
        &mut self,
        variable: &str,
        label: &str,
        success: &str,
        failure: &str,
        probe: Probe<'_>,
    ) -> Result<()> {
        if self.get(variable).is_some() {
            return Ok(());
        }
        let quiet = self.checks_quiet();
        if !quiet {
            println!("-- {label}");
        }
        let (compiled, output) = self.try_compile(&probe)?;
        let mut passed = compiled;
        for pattern in probe.fail_regexes {
            if cmake_regex(pattern)?.is_match(&output) {
                passed = false;
                break;
            }
        }
        self.cache_set(variable, if passed { "1" } else { "" }, "INTERNAL", label);
        if !quiet {
            println!("-- {label} - {}", if passed { success } else { failure });
        }
        Ok(())
    }

    /// `check_include_file(<include> <variable>)` and the CXX variant.
    fn check_include_file(&mut self, args: &[String], language: Language) -> Result<()> {
        need(args, 2)?;
        let (header, variable) = (args[0].clone(), args[1].clone());
        let source = format!("#include <{header}>\n\nint main(void) {{\n  return 0;\n}}\n");
        self.perform_check(
            &variable,
            &format!("Looking for {header}"),
            "found",
            "not found",
            Probe {
                language,
                source: &source,
                ..Probe::default()
            },
        )
    }

    /// `check_include_files(<include>... <variable>)`: `args[0]` is a
    /// `;`-separated list of headers, all included together.
    fn check_include_files(&mut self, args: &[String], language: Language) -> Result<()> {
        need(args, 2)?;
        let headers: Vec<String> = split_list(&args[0])
            .into_iter()
            .filter(|header| !header.is_empty())
            .collect();
        let variable = args[1].clone();
        let language = keyword_language(&args[2..], language)?;
        let mut source: String = headers
            .iter()
            .map(|header| format!("#include <{header}>\n"))
            .collect();
        source.push_str("\nint main(void) {\n  return 0;\n}\n");
        let label = format!(
            "Looking for {} include files {}",
            headers.len(),
            headers.join(", ")
        );
        self.perform_check(
            &variable,
            &label,
            "found",
            "not found",
            Probe {
                language,
                source: &source,
                ..Probe::default()
            },
        )
    }

    /// `check_function_exists(<function> <variable>)`: links a program that
    /// declares and calls the function without including its header.
    fn check_function_exists(&mut self, args: &[String]) -> Result<()> {
        need(args, 2)?;
        let (function, variable) = (args[0].clone(), args[1].clone());
        self.perform_check(
            &variable,
            &format!("Looking for {function}"),
            "found",
            "not found",
            Probe {
                link: true,
                source: &function_probe(&function),
                ..Probe::default()
            },
        )
    }

    /// `check_library_exists(<library> <function> <location> <variable>)`:
    /// links a program calling `<function>` against `<library>` with
    /// `-L<location>`.
    fn check_library_exists(&mut self, args: &[String]) -> Result<()> {
        need(args, 4)?;
        let (library, function, location, variable) = (
            args[0].clone(),
            args[1].clone(),
            args[2].clone(),
            args[3].clone(),
        );
        let mut link_items = Vec::new();
        if !location.is_empty() {
            link_items.push(format!("-L{location}"));
        }
        link_items.push(if library.starts_with('-') || library.contains('/') {
            library.clone()
        } else {
            format!("-l{library}")
        });
        let (source, label) = if function.is_empty() {
            (
                "int main(void) {\n  return 0;\n}\n".to_owned(),
                format!("Looking for {library}"),
            )
        } else {
            (
                function_probe(&function),
                format!("Looking for {function} in {library}"),
            )
        };
        self.perform_check(
            &variable,
            &label,
            "found",
            "not found",
            Probe {
                link: true,
                source: &source,
                extra_link_items: &link_items,
                ..Probe::default()
            },
        )
    }

    /// `check_symbol_exists(<symbol> <files> <variable>)`: `<files>` is a
    /// `;`-separated list of headers; the probe takes the symbol's address
    /// unless it is a macro.
    fn check_symbol_exists(&mut self, args: &[String], language: Language) -> Result<()> {
        need(args, 3)?;
        let (symbol, files, variable) = (args[0].clone(), args[1].clone(), args[2].clone());
        let mut source = String::new();
        for header in split_list(&files) {
            if !header.is_empty() {
                source.push_str(&format!("#include <{header}>\n"));
            }
        }
        source.push_str(&format!(
            "\nint main(int argc, char** argv) {{\n  (void)argv;\n#ifndef {symbol}\n  return ((int*)(&{symbol}))[argc];\n#else\n  (void)argc;\n  return 0;\n#endif\n}}\n"
        ));
        self.perform_check(
            &variable,
            &format!("Looking for {symbol}"),
            "found",
            "not found",
            Probe {
                language,
                link: true,
                source: &source,
                ..Probe::default()
            },
        )
    }

    /// `check_struct_has_member(<struct> <member> <headers> <variable>
    /// [LANGUAGE <language>])`.
    fn check_struct_has_member(&mut self, args: &[String]) -> Result<()> {
        need(args, 4)?;
        let (structure, member, files, variable) = (
            args[0].clone(),
            args[1].clone(),
            args[2].clone(),
            args[3].clone(),
        );
        let language = keyword_language(&args[4..], Language::C)?;
        let mut source = String::new();
        for header in split_list(&files) {
            if !header.is_empty() {
                source.push_str(&format!("#include <{header}>\n"));
            }
        }
        source.push_str(&format!(
            "\nint main(void) {{\n  (void)sizeof((({structure}*)0)->{member});\n  return 0;\n}}\n"
        ));
        self.perform_check(
            &variable,
            &format!("Performing Test {variable}"),
            "Success",
            "Failed",
            Probe {
                language,
                source: &source,
                ..Probe::default()
            },
        )
    }

    /// `check_c_source_compiles`/`check_cxx_source_compiles` (`<source>
    /// <variable> [FAIL_REGEX <regex>...]`): compiles and links the given
    /// program.
    fn check_source_compiles(&mut self, args: &[String], language: Language) -> Result<()> {
        need(args, 2)?;
        let (source, variable) = (args[0].clone(), args[1].clone());
        let mut fail_regexes: &[String] = &[];
        if args.len() > 2 {
            if args[2] != "FAIL_REGEX" {
                return fail(format!(
                    "check_{}source_compiles: unexpected argument {:?}",
                    language.name().to_ascii_lowercase(),
                    args[2]
                ));
            }
            need(&args[3..], 1)?;
            fail_regexes = &args[3..];
        }
        self.perform_check(
            &variable,
            &format!("Performing Test {variable}"),
            "Success",
            "Failed",
            Probe {
                language,
                link: true,
                source: &source,
                fail_regexes,
                ..Probe::default()
            },
        )
    }

    /// `check_c_compiler_flag`/`check_cxx_compiler_flag` (`<flag> <variable>`):
    /// compiles a trivial program with the flag, also failing on compiler
    /// output that indicates the flag was not understood.
    fn check_compiler_flag(&mut self, args: &[String], language: Language) -> Result<()> {
        need(args, 2)?;
        let (flag, variable) = (args[0].clone(), args[1].clone());
        let fail_regexes: Vec<String> = FLAG_FAIL_PATTERNS
            .iter()
            .map(|pattern| (*pattern).to_owned())
            .collect();
        self.perform_check(
            &variable,
            &format!("Performing Test {variable}"),
            "Success",
            "Failed",
            Probe {
                language,
                source: "int main(void) {\n  return 0;\n}\n",
                extra_flags: std::slice::from_ref(&flag),
                fail_regexes: &fail_regexes,
                ..Probe::default()
            },
        )
    }

    /// `check_linker_flag(<language> <flag> <variable>)`: links a trivial
    /// program with the flag added to the link line.
    fn check_linker_flag(&mut self, args: &[String]) -> Result<()> {
        need(args, 3)?;
        let language = match args[0].as_str() {
            "C" => Language::C,
            "CXX" => Language::Cxx,
            other => return fail(format!("check_linker_flag: unsupported language {other:?}")),
        };
        let (flag, variable) = (args[1].clone(), args[2].clone());
        let fail_regexes: Vec<String> = FLAG_FAIL_PATTERNS
            .iter()
            .map(|pattern| (*pattern).to_owned())
            .collect();
        self.perform_check(
            &variable,
            &format!("Performing Test {variable}"),
            "Success",
            "Failed",
            Probe {
                language,
                link: true,
                source: "int main(void) {\n  return 0;\n}\n",
                extra_link_items: std::slice::from_ref(&flag),
                fail_regexes: &fail_regexes,
                ..Probe::default()
            },
        )
    }

    /// `check_type_size(<type> <variable> [BUILTIN_TYPES_ONLY] [LANGUAGE
    /// <language>])`. Determines `sizeof(<type>)` numerically with
    /// compile-time probes: the typedef below is valid exactly when
    /// `sizeof(<type>) >= N`, giving a monotone predicate that a binary
    /// search resolves to the exact size.
    fn check_type_size(&mut self, args: &[String]) -> Result<()> {
        need(args, 2)?;
        let (type_name, variable) = (args[0].clone(), args[1].clone());
        let have_variable = format!("HAVE_{variable}");
        if self.get(&variable).is_some() || self.get(&have_variable).is_some() {
            return Ok(());
        }
        let mut language = Language::C;
        let mut builtin_only = false;
        let mut index = 2;
        while index < args.len() {
            match args[index].as_str() {
                "BUILTIN_TYPES_ONLY" => builtin_only = true,
                "LANGUAGE" => {
                    index += 1;
                    language = match args.get(index).map(String::as_str) {
                        Some("C") => Language::C,
                        Some("CXX") => Language::Cxx,
                        other => {
                            return fail(format!(
                                "check_type_size: unsupported LANGUAGE {other:?}"
                            ));
                        }
                    };
                }
                other => return fail(format!("check_type_size: unexpected argument {other:?}")),
            }
            index += 1;
        }
        let mut headers = String::new();
        if !builtin_only {
            headers.push_str("#include <sys/types.h>\n#include <stdint.h>\n#include <stddef.h>\n");
        }
        if let Some(extra) = self.get("CMAKE_EXTRA_INCLUDE_FILES") {
            for header in split_list(&extra) {
                if !header.is_empty() {
                    headers.push_str(&format!("#include <{header}>\n"));
                }
            }
        }
        let quiet = self.checks_quiet();
        if !quiet {
            println!("-- Check size of {type_name}");
        }
        let exists = format!(
            "{headers}\ntypedef char cmake_sizeof_exists_[(sizeof({type_name}) > 0) ? 1 : -1];\nint main(void) {{\n  return 0;\n}}\n"
        );
        if !self
            .try_compile(&Probe {
                language,
                source: &exists,
                ..Probe::default()
            })?
            .0
        {
            self.cache_set(&variable, "0", "INTERNAL", "Size result");
            self.cache_set(have_variable, "", "INTERNAL", "Size check");
            self.set(
                format!("{variable}_CODE"),
                format!("/* #undef {variable} */\n"),
            );
            if !quiet {
                println!("-- Check size of {type_name} - failed");
            }
            return Ok(());
        }
        const LIMIT: u64 = 1 << 20;
        let predicate = |bound: u64| {
            format!(
                "{headers}\ntypedef char cmake_sizeof_probe_[(sizeof({type_name}) >= ({bound})) ? 1 : -1];\nint main(void) {{\n  return 0;\n}}\n"
            )
        };
        let probe = |bound: u64| -> Result<bool> {
            Ok(self
                .try_compile(&Probe {
                    language,
                    source: &predicate(bound),
                    ..Probe::default()
                })?
                .0)
        };
        let mut low: u64 = 1;
        while low < LIMIT && probe(low * 2)? {
            low *= 2;
        }
        if low >= LIMIT {
            return fail(format!(
                "check_type_size: could not determine size of {type_name}"
            ));
        }
        let mut high = low * 2;
        while high - low > 1 {
            let mid = low + (high - low) / 2;
            if probe(mid)? {
                low = mid;
            } else {
                high = mid;
            }
        }
        self.cache_set(&variable, low.to_string(), "INTERNAL", "Size result");
        self.cache_set(have_variable, "1", "INTERNAL", "Size check");
        self.set(
            format!("{variable}_CODE"),
            format!(
                "/* The size of `{type_name}', as computed by sizeof. */\n#define {variable} {low}\n"
            ),
        );
        if !quiet {
            println!("-- Check size of {type_name} - done");
        }
        Ok(())
    }

    /// `cmake_dependent_option(<option> <doc> <default> <depends> <force>)`:
    /// when every condition in `<depends>` holds, behaves like `option()`;
    /// otherwise stores the current value in the cache for preservation and
    /// sets `<option>` to `<force>` in the current scope.
    fn cmake_dependent_option(&mut self, args: &[String]) -> Result<()> {
        need(args, 5)?;
        let option = args[0].clone();
        let default = args[2].clone();
        let depends = args[3].clone();
        let force = args[4].clone();
        let mut available = true;
        for item in split_list(&depends) {
            let tokens: Vec<Argument> = item
                .split_whitespace()
                .map(|text| Argument {
                    text: text.to_owned(),
                    form: Form::Bare,
                })
                .collect();
            if !tokens.is_empty() && !self.condition(&tokens)? {
                available = false;
            }
        }
        if available {
            let value = self
                .variables()
                .get(&option)
                .cloned()
                .or_else(|| self.cache_value(&option))
                .unwrap_or(default);
            self.cache_set(option, value, "BOOL", &args[1]);
        } else {
            if let Some(current) = self.get(&option) {
                self.cache_set(option.clone(), current, "INTERNAL", &args[1]);
            }
            self.set(&option, force);
        }
        Ok(())
    }

    /// `cmake_push_check_state([RESET])`: snapshots the `CMAKE_REQUIRED_*`
    /// variables so `cmake_pop_check_state()` can restore them.
    fn cmake_push_check_state(&mut self, args: &[String]) -> Result<()> {
        if args.len() > 1 || args.first().is_some_and(|arg| arg != "RESET") {
            return fail("cmake_push_check_state expects at most a RESET argument");
        }
        let depth = self.check_state_depth() + 1;
        let prefix = format!("_CMAKE_CHECK_STATE_{depth}_");
        let mut defined = Vec::new();
        for name in CHECK_STATE_VARIABLES {
            if let Some(value) = self.variables().get(*name) {
                defined.push(*name);
                self.set(format!("{prefix}{name}"), value.clone());
            }
        }
        self.set(format!("{prefix}DEFINED"), defined.join(";"));
        self.set("_CMAKE_CHECK_STATE_DEPTH", depth.to_string());
        if !args.is_empty() {
            for name in CHECK_STATE_VARIABLES {
                self.variables_mut().remove(*name);
            }
        }
        Ok(())
    }

    /// `cmake_pop_check_state()`: restores the most recently pushed
    /// `CMAKE_REQUIRED_*` snapshot; a no-op when the stack is empty.
    fn cmake_pop_check_state(&mut self, args: &[String]) -> Result<()> {
        if !args.is_empty() {
            return fail("cmake_pop_check_state takes no arguments");
        }
        let depth = self.check_state_depth();
        if depth == 0 {
            return Ok(());
        }
        let prefix = format!("_CMAKE_CHECK_STATE_{depth}_");
        let defined: Vec<String> = self
            .get(&format!("{prefix}DEFINED"))
            .map(|value| split_list(&value))
            .unwrap_or_default();
        for name in CHECK_STATE_VARIABLES {
            if defined.iter().any(|saved| saved.as_str() == *name) {
                if let Some(value) = self.get(&format!("{prefix}{name}")) {
                    self.set(*name, value);
                }
            } else {
                self.variables_mut().remove(*name);
            }
            self.variables_mut().remove(&format!("{prefix}{name}"));
        }
        self.variables_mut().remove(&format!("{prefix}DEFINED"));
        self.set("_CMAKE_CHECK_STATE_DEPTH", (depth - 1).to_string());
        Ok(())
    }

    /// `cmake_reset_check_state()`: clears all `CMAKE_REQUIRED_*` variables.
    fn cmake_reset_check_state(&mut self, args: &[String]) -> Result<()> {
        if !args.is_empty() {
            return fail("cmake_reset_check_state takes no arguments");
        }
        for name in CHECK_STATE_VARIABLES {
            self.variables_mut().remove(*name);
        }
        Ok(())
    }

    fn check_state_depth(&self) -> usize {
        self.variables()
            .get("_CMAKE_CHECK_STATE_DEPTH")
            .and_then(|value| value.parse().ok())
            .unwrap_or(0)
    }

    /// Sets the standard `CMAKE_INSTALL_*DIR` cache variables like
    /// GNUInstallDirs.cmake. User-provided cache values are preserved, and
    /// `CMAKE_INSTALL_FULL_*` variants are derived from the prefix.
    fn gnu_install_dirs(&mut self) -> Result<()> {
        let prefix = self
            .get("CMAKE_INSTALL_PREFIX")
            .unwrap_or_else(|| "/usr/local".to_owned());
        let project = self.get("PROJECT_NAME").unwrap_or_default();
        let docdir = if project.is_empty() {
            "share/doc".to_owned()
        } else {
            format!("share/doc/{project}")
        };
        for (name, default) in [
            ("BINDIR", "bin"),
            ("SBINDIR", "sbin"),
            ("LIBEXECDIR", "libexec"),
            ("SYSCONFDIR", "etc"),
            ("SHAREDSTATEDIR", "com"),
            ("LOCALSTATEDIR", "var"),
            ("RUNSTATEDIR", "var/run"),
            ("LIBDIR", "lib"),
            ("INCLUDEDIR", "include"),
            ("OLDINCLUDEDIR", "/usr/include"),
            ("DATAROOTDIR", "share"),
            ("DATADIR", "share"),
            ("INFODIR", "share/info"),
            ("LOCALEDIR", "share/locale"),
            ("MANDIR", "share/man"),
            ("DOCDIR", docdir.as_str()),
        ] {
            let variable = format!("CMAKE_INSTALL_{name}");
            let value = self
                .cache
                .entry(variable)
                .or_insert_with(|| crate::cache::CacheEntry::new(default, "PATH", ""))
                .value
                .clone();
            let full = if Path::new(&value).is_absolute() {
                value
            } else {
                format!("{prefix}/{value}")
            };
            self.set(format!("CMAKE_INSTALL_FULL_{name}"), full);
        }
        Ok(())
    }
}

impl Language {
    fn name(self) -> &'static str {
        match self {
            Self::C => "C",
            Self::Cxx => "CXX",
        }
    }
}

/// Whether `include()` should treat `name` as a module name rather than a
/// file path.
fn is_module_name(name: &str) -> bool {
    !name.ends_with(".cmake") && !name.contains('/') && !name.contains('\\')
}

/// Resolves a possibly-relative path against `base`.
fn absolute(path: &str, base: &Path) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

/// Parses a trailing `LANGUAGE <language>` argument pair.
fn keyword_language(args: &[String], default: Language) -> Result<Language> {
    match args {
        [] => Ok(default),
        [keyword, language] if keyword == "LANGUAGE" => match language.as_str() {
            "C" => Ok(Language::C),
            "CXX" => Ok(Language::Cxx),
            other => fail(format!("unsupported LANGUAGE {other:?}")),
        },
        _ => fail(format!("unexpected arguments: {}", args.join(" "))),
    }
}

/// The classic CheckFunctionExists probe: declares the function itself (no
/// headers) and calls it, so it succeeds exactly when the symbol links.
fn function_probe(function: &str) -> String {
    format!(
        "#ifdef __cplusplus\nextern \"C\"\n#endif\nchar {function}(void);\nint main(int argc, char** argv) {{\n  (void)argc;\n  (void)argv;\n  return {function}();\n}}\n"
    )
}
