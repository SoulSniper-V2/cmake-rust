use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use regex::{Captures, Regex};

use super::condition::{cmake_regex, version_order};
use super::find::FindKind;
use super::{Interpreter, is_false, need, path_string};
use crate::eval::Directory;
use crate::graph::Tools;
use crate::syntax::{Command, split_list};
use crate::{Result, fail, write_changed};

const COMMANDS: &[&str] = &[
    "add_subdirectory",
    "cmake_minimum_required",
    "cmake_parse_arguments",
    "cmake_policy",
    "configure_file",
    "execute_process",
    "file",
    "find_file",
    "find_library",
    "find_package",
    "find_path",
    "find_program",
    "get_cmake_property",
    "get_filename_component",
    "get_property",
    "include",
    "include_guard",
    "list",
    "mark_as_advanced",
    "math",
    "message",
    "option",
    "project",
    "set",
    "set_directory_properties",
    "set_property",
    "string",
    "try_compile",
    "unset",
];

const POLICIES: &[(&str, &str)] = &[
    ("CMP0007", "2.6"),
    ("CMP0012", "2.8"),
    ("CMP0057", "3.3"),
    ("CMP0124", "3.21"),
    ("CMP0133", "3.24"),
    ("CMP0134", "3.24"),
];

pub(super) fn knows_policy(name: &str) -> bool {
    POLICIES.iter().any(|(policy, _)| *policy == name)
}

impl Interpreter {
    pub(super) fn knows_command(&self, name: &str) -> bool {
        COMMANDS.contains(&name)
            || self.functions.contains_key(name)
            || self.knows_graph_command(name)
            || self.knows_native_command(name)
    }

    pub(super) fn execute(&mut self, command: &Command, args: &[String]) -> Result<()> {
        match command.name.as_str() {
            "set" => self.set_command(args),
            "unset" => self.unset_command(args),
            "option" => {
                need(args, 2)?;
                if args.len() > 3 {
                    return fail("option() accepts a name, description and default");
                }
                if !self.variables().contains_key(&args[0])
                    && self
                        .cache
                        .get(&args[0])
                        .is_none_or(|entry| entry.kind == "UNINITIALIZED")
                {
                    let value = args
                        .get(2)
                        .filter(|value| !is_false(value))
                        .map_or("OFF", |_| "ON");
                    let value = self.cache_value(&args[0]).unwrap_or_else(|| value.into());
                    self.cache_set(&args[0], value, "BOOL", &args[1]);
                }
                Ok(())
            }
            "cmake_minimum_required" => {
                need(args, 2)?;
                if args[0] != "VERSION"
                    || args.len() > 3
                    || (args.len() == 3 && args[2] != "FATAL_ERROR")
                {
                    return fail("expected VERSION <min> or VERSION <min>...<max>");
                }
                let (version, policy_version) =
                    args[1].split_once("...").unwrap_or((&args[1], &args[1]));
                if version_order(version, "3.20.0").is_gt() {
                    return fail(format!(
                        "requested CMake {version}; this implementation targets a subset of 3.20"
                    ));
                }
                self.set("CMAKE_MINIMUM_REQUIRED_VERSION", version);
                self.set_policy_version(policy_version);
                Ok(())
            }
            "cmake_parse_arguments" => self.parse_arguments(args),
            "cmake_policy" => self.cmake_policy(args),
            "project" => self.project(args),
            "math" => self.math(args),
            "message" => {
                let Some(mode) = args.first() else {
                    return Ok(());
                };
                let text = args[1..].join("");
                match mode.as_str() {
                    "FATAL_ERROR" | "SEND_ERROR" => fail(text),
                    "WARNING" | "AUTHOR_WARNING" | "DEPRECATION" => {
                        eprintln!("{mode}: {text}");
                        Ok(())
                    }
                    "STATUS" | "NOTICE" | "VERBOSE" | "DEBUG" | "TRACE" | "CHECK_START"
                    | "CHECK_PASS" | "CHECK_FAIL" => {
                        if matches!(
                            mode.as_str(),
                            "STATUS" | "NOTICE" | "CHECK_START" | "CHECK_PASS" | "CHECK_FAIL"
                        ) {
                            println!("-- {text}");
                        }
                        Ok(())
                    }
                    _ => {
                        eprintln!("{}", args.join(""));
                        Ok(())
                    }
                }
            }
            "mark_as_advanced" => {
                let (variables, value, overwrite) = match args.first().map(String::as_str) {
                    Some("CLEAR") => (&args[1..], "0", true),
                    Some("FORCE") => (&args[1..], "1", true),
                    _ => (args, "1", false),
                };
                for variable in variables {
                    if let Some(entry) = self.cache.get_mut(variable) {
                        if overwrite || entry.property("ADVANCED").is_none() {
                            entry.set_property("ADVANCED", value.into());
                        }
                    }
                }
                Ok(())
            }
            "include" => self.include_command(args),
            "add_subdirectory" => self.subdirectory(args),
            "file" => self.file(args),
            "configure_file" => self.configure_file(args),
            "execute_process" => self.execute_process(args),
            "list" => self.list(args),
            "string" => self.string(args),
            "get_property" => self.get_property(args),
            "get_cmake_property" => self.get_cmake_property(args),
            "set_directory_properties" => self.set_directory_properties(args),
            "set_property" => self.set_property(args),
            "get_filename_component" => self.get_filename_component(args),
            "find_package" => self.find_package(args),
            "find_file" => self.find_item(FindKind::File, args),
            "find_library" => self.find_item(FindKind::Library, args),
            "find_path" => self.find_item(FindKind::Path, args),
            "find_program" => self.find_item(FindKind::Program, args),
            "try_compile" => self.try_compile_command(args),
            name => {
                if self.graph_command(command, args)? || self.native_command(name, args)? {
                    Ok(())
                } else {
                    fail(format!("unsupported command {name}"))
                }
            }
        }
    }

    fn set_policy_version(&mut self, version: &str) {
        let minimum = self.get("CMAKE_POLICY_VERSION_MINIMUM").unwrap_or_default();
        let version = if version_order(&minimum, version).is_gt() {
            &minimum
        } else {
            version
        };
        for &(policy, introduced) in POLICIES {
            if version_order(version, introduced).is_ge() {
                self.policies.insert(policy.into(), "NEW".into());
            } else {
                self.policies.remove(policy);
            }
        }
    }

    fn cmake_policy(&mut self, args: &[String]) -> Result<()> {
        match args {
            [verb] if verb == "PUSH" => self.policy_stack.push(self.policies.clone()),
            [verb] if verb == "POP" => {
                if self.policy_stack.len() <= self.policy_barrier {
                    return fail("cmake_policy POP without matching PUSH");
                }
                self.policies = self.policy_stack.pop().expect("policy stack above barrier");
            }
            [verb, policy, variable] if verb == "GET" && knows_policy(policy) => {
                self.set(
                    variable,
                    self.policies.get(policy).cloned().unwrap_or_default(),
                );
            }
            [verb, policy, state]
                if verb == "SET"
                    && knows_policy(policy)
                    && (state == "NEW" || (policy == "CMP0133" && state == "OLD")) =>
            {
                self.policies.insert(policy.clone(), state.clone());
            }
            _ => return fail(format!("unsupported cmake_policy arguments {args:?}")),
        }
        Ok(())
    }

    fn set_command(&mut self, args: &[String]) -> Result<()> {
        need(args, 1)?;
        let name = &args[0];
        let values = &args[1..];
        if values.last().is_some_and(|value| value == "PARENT_SCOPE") {
            let len = self.scopes.len();
            if len < 2 {
                eprintln!("Warning: set({name} PARENT_SCOPE) has no parent scope");
                return Ok(());
            }
            let scope = &mut self.scopes[len - 2];
            if values.len() == 1 {
                scope.remove(name);
            } else {
                scope.insert(name.clone(), values[..values.len() - 1].join(";"));
            }
        } else {
            let force = values.last().is_some_and(|value| value == "FORCE");
            let cache_index = values.len().checked_sub(if force { 4 } else { 3 });
            if let Some(index) = cache_index.filter(|index| values[*index] == "CACHE") {
                let kind = &values[index + 1];
                let entry = self.cache.get(name);
                let replace = force || kind == "INTERNAL";
                if replace || entry.is_none_or(|entry| entry.kind == "UNINITIALIZED") {
                    let mut value = entry
                        .filter(|_| !replace)
                        .map_or_else(|| values[..index].join(";"), |entry| entry.value.clone());
                    if entry.is_some_and(|entry| entry.kind == "UNINITIALIZED")
                        && matches!(kind.as_str(), "PATH" | "FILEPATH")
                    {
                        let cwd = env::current_dir()?;
                        value = split_list(&value)
                            .into_iter()
                            .map(|item| {
                                if item.is_empty() || item.ends_with("-NOTFOUND") {
                                    item
                                } else {
                                    path_string(&normalize(&cwd.join(item)))
                                }
                            })
                            .collect::<Vec<_>>()
                            .join(";");
                    }
                    self.cache_set(name, value, kind, &values[index + 2]);
                    self.variables_mut().remove(name);
                }
            } else if values.is_empty() {
                self.variables_mut().remove(name);
            } else {
                self.set(name, values.join(";"));
            }
        }
        Ok(())
    }

    fn unset_command(&mut self, args: &[String]) -> Result<()> {
        need(args, 1)?;
        match args.get(1).map(String::as_str) {
            None => {
                self.variables_mut().remove(&args[0]);
            }
            Some("CACHE") => {
                self.cache.remove(&args[0]);
            }
            Some("PARENT_SCOPE") => {
                let len = self.scopes.len();
                if len >= 2 {
                    self.scopes[len - 2].remove(&args[0]);
                }
            }
            Some(mode) => return fail(format!("unsupported unset mode {mode}")),
        }
        Ok(())
    }

    fn project(&mut self, args: &[String]) -> Result<()> {
        need(args, 1)?;
        let name = &args[0];
        let mut rest = args[1..].iter().peekable();
        let mut version = "";
        let mut description = "";
        let mut url = "";
        let mut languages = Vec::new();
        while let Some(argument) = rest.next() {
            match argument.as_str() {
                "VERSION" => version = rest.next().ok_or("project VERSION needs a value")?,
                "DESCRIPTION" => {
                    description = rest.next().ok_or("project DESCRIPTION needs a value")?
                }
                "HOMEPAGE_URL" => url = rest.next().ok_or("project HOMEPAGE_URL needs a value")?,
                "LANGUAGES" => {}
                "C" | "CXX" | "ASM" | "NONE" => languages.push(argument.as_str()),
                value => return fail(format!("unsupported project argument {value}")),
            }
        }
        if languages.is_empty() {
            languages.extend(["C", "CXX"]);
        }
        let source = path_string(&self.directory().source);
        let binary = path_string(&self.directory().binary);
        let top_level = self.directories.len() == 1;
        self.set("PROJECT_NAME", name);
        for prefix in ["PROJECT", name] {
            for (key, value) in [
                ("SOURCE_DIR", source.as_str()),
                ("BINARY_DIR", binary.as_str()),
                ("IS_TOP_LEVEL", if top_level { "ON" } else { "OFF" }),
                ("VERSION", version),
                ("DESCRIPTION", description),
                ("HOMEPAGE_URL", url),
            ] {
                self.set(format!("{prefix}_{key}"), value);
            }
            let mut versions = version.split('.');
            for part in ["MAJOR", "MINOR", "PATCH", "TWEAK"] {
                self.set(
                    format!("{prefix}_VERSION_{part}"),
                    versions.next().unwrap_or_default(),
                );
            }
        }
        if top_level {
            self.set("CMAKE_PROJECT_NAME", name);
            self.set("CMAKE_PROJECT_VERSION", version);
        }
        for language in languages {
            if language == "NONE" {
                continue;
            }
            let compiler = match language {
                "CXX" => self.tools.cxx.clone(),
                _ => self.tools.cc.clone(),
            };
            let id = Tools::compiler_id(&compiler)?;
            self.set(format!("CMAKE_{language}_COMPILER"), compiler);
            self.set(format!("CMAKE_{language}_COMPILER_ID"), id);
            self.set(format!("CMAKE_{language}_COMPILER_LOADED"), "1");
            let (standards, dialect) = match language {
                "C" => (&["90", "99", "11", "17"][..], "c"),
                "CXX" => (&["98", "11", "14", "17", "20"][..], "c++"),
                _ => (&[][..], ""),
            };
            for version in standards {
                self.set(
                    format!("CMAKE_{language}{version}_STANDARD_COMPILE_OPTION"),
                    format!("-std={dialect}{version}"),
                );
            }
        }
        self.set("CMAKE_SIZEOF_VOID_P", (usize::BITS / 8).to_string());
        Ok(())
    }

    fn subdirectory(&mut self, args: &[String]) -> Result<()> {
        need(args, 1)?;
        let parent = self.directory().clone();
        let relative = Path::new(&args[0]);
        let source = fs::canonicalize(parent.source.join(relative))?;
        if self
            .directories
            .iter()
            .any(|directory| directory.source == source)
        {
            return fail(format!(
                "recursive add_subdirectory of {}",
                source.display()
            ));
        }
        if args
            .iter()
            .skip(1)
            .any(|arg| arg == "EXCLUDE_FROM_ALL" || arg == "SYSTEM")
        {
            return fail("add_subdirectory EXCLUDE_FROM_ALL and SYSTEM are not implemented");
        }
        if args.len() > 2 {
            return fail("expected source directory and optional binary directory");
        }
        let binary = if let Some(path) = args.get(1) {
            parent.binary.join(path)
        } else if let Ok(path) = source.strip_prefix(&parent.source) {
            parent.binary.join(path)
        } else {
            return fail("add_subdirectory outside its source parent requires a binary directory");
        };
        fs::create_dir_all(&binary)?;
        let binary = fs::canonicalize(binary)?;
        if !self.graph.directories.insert(binary.clone()) {
            return fail(format!(
                "binary directory already in use: {}",
                binary.display()
            ));
        }
        let input = source.join("CMakeLists.txt");
        self.directories.push(Directory {
            source,
            binary,
            ..parent
        });
        self.push_scope();
        let result = self.run_include(input, false);
        self.scopes.pop();
        self.directories.pop();
        result
    }

    fn file(&mut self, args: &[String]) -> Result<()> {
        need(args, 1)?;
        let source = &self.directory().source;
        match args[0].as_str() {
            "READ" => {
                need(args, 3)?;
                let input = source.join(&args[1]);
                let bytes = fs::read(&input)?;
                let (mut offset, mut limit) = (0_usize, usize::MAX);
                for pair in args[3..].chunks(2) {
                    match pair {
                        [key, value] if key == "OFFSET" => offset = value.parse()?,
                        [key, value] if key == "LIMIT" => limit = value.parse()?,
                        _ => return fail("file(READ) only supports OFFSET and LIMIT"),
                    }
                }
                let end = offset.saturating_add(limit).min(bytes.len());
                let data = if offset < end {
                    &bytes[offset..end]
                } else {
                    &[]
                };
                self.graph.inputs.insert(input);
                self.set(
                    &args[2],
                    String::from_utf8(data.to_vec())?.replace("\r\n", "\n"),
                );
            }
            "STRINGS" => return self.file_strings(args),
            "RENAME" => {
                need(args, 3)?;
                if args.len() != 3 {
                    return fail("file(RENAME) options are not implemented");
                }
                fs::rename(source.join(&args[1]), source.join(&args[2]))?;
            }
            "MAKE_DIRECTORY" => {
                for directory in &args[1..] {
                    fs::create_dir_all(source.join(directory))?;
                }
            }
            "WRITE" | "APPEND" => {
                need(args, 2)?;
                let path = source.join(&args[1]);
                fs::create_dir_all(path.parent().ok_or("file has no parent directory")?)?;
                let text = args[2..].concat();
                if args[0] == "APPEND" {
                    use std::io::Write as _;
                    let mut file = fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(path)?;
                    file.write_all(text.as_bytes())?;
                } else {
                    write_changed(&path, text.as_bytes())?;
                }
            }
            "TO_CMAKE_PATH" => {
                need(args, 3)?;
                let paths = env::split_paths(&args[1])
                    .map(|path| path_string(&path))
                    .collect::<Vec<_>>();
                self.set(&args[2], paths.join(";"));
            }
            operation => return fail(format!("unsupported file operation {operation}")),
        }
        Ok(())
    }

    fn file_strings(&mut self, args: &[String]) -> Result<()> {
        need(args, 3)?;
        let input = self.directory().source.join(&args[1]);
        let bytes = fs::read(&input)?;
        let mut count = usize::MAX;
        let mut input_limit = usize::MAX;
        let mut minimum_length = 0;
        let mut maximum_length = usize::MAX;
        let mut pattern = None;
        for pair in args[3..].chunks(2) {
            match pair {
                [key, value] if key == "LIMIT_COUNT" => count = value.parse()?,
                [key, value] if key == "LIMIT_INPUT" => input_limit = value.parse()?,
                [key, value] if key == "LENGTH_MINIMUM" => minimum_length = value.parse()?,
                [key, value] if key == "LENGTH_MAXIMUM" => maximum_length = value.parse()?,
                [key, value] if key == "REGEX" => pattern = Some(cmake_regex(value)?),
                _ => return fail("unsupported file(STRINGS) option"),
            }
        }
        let items = bytes[..bytes.len().min(input_limit)]
            .split(|byte| *byte == b'\n')
            .filter_map(|line| std::str::from_utf8(line).ok())
            .filter_map(|line| {
                let line = line.trim_end_matches('\r');
                (line.len() >= minimum_length
                    && line.len() <= maximum_length
                    && !line.contains('\0')
                    && pattern
                        .as_ref()
                        .is_none_or(|pattern| pattern.is_match(line)))
                .then(|| line.replace(';', "\\;"))
            })
            .take(count)
            .collect::<Vec<_>>();
        self.graph.inputs.insert(input);
        self.set(&args[2], items.join(";"));
        Ok(())
    }

    fn configure_file(&mut self, args: &[String]) -> Result<()> {
        need(args, 2)?;
        let source = self.directory().source.join(&args[0]);
        let mut output = self.directory().binary.join(&args[1]);
        if output.is_dir() {
            output.push(source.file_name().ok_or("input file missing filename")?);
        }
        if source == output {
            return fail("configure_file input and output must differ");
        }
        for flag in &args[2..] {
            if !matches!(flag.as_str(), "@ONLY" | "ESCAPE_QUOTES" | "COPYONLY") {
                return fail(format!("unsupported configure_file option {flag}"));
            }
        }
        self.graph.inputs.insert(source.clone());
        let data = fs::read(source)?;
        if args.iter().any(|arg| arg == "COPYONLY") {
            return write_changed(&output, &data);
        }
        let at_only = args.iter().any(|arg| arg == "@ONLY");
        let escape = args.iter().any(|arg| arg == "ESCAPE_QUOTES");
        let template = String::from_utf8(data)?.replace("\r\n", "\n");
        let variable = Regex::new(r"@([A-Za-z0-9_./+-]+)@")?;
        let define = Regex::new(r"^([ \t]*)#([ \t]*)cmakedefine(01)?[ \t]+([^ \t\n]+)([^\n]*)")?;
        let mut result = String::new();
        for line in template.split_inclusive('\n') {
            let expanded = if at_only {
                line.to_owned()
            } else {
                self.expand_config_variables(line, escape)?
            };
            let mut output = variable
                .replace_all(&expanded, |group: &Captures<'_>| {
                    let value = self.get(&group[1]).unwrap_or_default();
                    if escape {
                        value.replace('"', "\\\"")
                    } else {
                        value
                    }
                })
                .into_owned();
            if let Some(parts) = define.captures(&output) {
                let suffix = if line.ends_with('\n') { "\n" } else { "" };
                let name = &parts[4];
                let enabled = self.get(name).is_some_and(|value| !is_false(&value));
                output = if parts.get(3).is_some() {
                    format!(
                        "{}#{}define {name} {}{}{suffix}",
                        &parts[1],
                        &parts[2],
                        usize::from(enabled),
                        &parts[5]
                    )
                } else if enabled {
                    format!(
                        "{}#{}define {name}{}{suffix}",
                        &parts[1], &parts[2], &parts[5]
                    )
                } else {
                    format!("{}/* #undef {name} */{suffix}", &parts[1])
                };
            }
            result.push_str(&output);
        }
        write_changed(&output, result.as_bytes())
    }

    fn expand_config_variables(&self, text: &str, escape: bool) -> Result<String> {
        let mut rest = text;
        let mut result = String::new();
        while let Some(start) = rest.find("${") {
            result.push_str(&rest[..start]);
            let variable = &rest[start + 2..];
            let end =
                super::reference_end(variable).ok_or("unterminated variable in configured file")?;
            let name = self.expand(&variable[..end])?;
            let value = self.get(&name).unwrap_or_default();
            if escape {
                result.push_str(&value.replace('"', "\\\""));
            } else {
                result.push_str(&value);
            }
            rest = &variable[end + 1..];
        }
        result.push_str(rest);
        Ok(result)
    }

    fn list(&mut self, args: &[String]) -> Result<()> {
        need(args, 2)?;
        let name = &args[1];
        let mut list = self
            .get(name)
            .filter(|value| !value.is_empty())
            .map_or_else(Vec::new, |value| split_list(&value));
        let join = |list: &[String]| {
            list.iter()
                .map(|value| value.replace(';', "\\;"))
                .collect::<Vec<_>>()
                .join(";")
        };
        match args[0].as_str() {
            "APPEND" => list.extend_from_slice(&args[2..]),
            "PREPEND" => list.splice(0..0, args[2..].iter().cloned()).for_each(drop),
            "INSERT" => {
                need(args, 4)?;
                let index = list_index(&args[2], list.len(), true)?;
                list.splice(index..index, args[3..].iter().cloned());
            }
            "FILTER" => match &args[2..] {
                [mode, operation, regex]
                    if matches!(mode.as_str(), "INCLUDE" | "EXCLUDE") && operation == "REGEX" =>
                {
                    let regex = cmake_regex(regex)?;
                    list.retain(|item| regex.is_match(item) == (mode == "INCLUDE"));
                }
                _ => return fail(format!("unsupported list(FILTER) arguments {args:?}")),
            },
            "TRANSFORM" => {
                need(args, 3)?;
                let (parameters, output) = match &args[3..] {
                    [initial @ .., marker, output] if marker == "OUTPUT_VARIABLE" => {
                        (initial, output)
                    }
                    initial => (initial, name),
                };
                match (args[2].as_str(), parameters) {
                    ("PREPEND", [value]) => {
                        list.iter_mut().for_each(|item| item.insert_str(0, value))
                    }
                    ("APPEND", [value]) => list.iter_mut().for_each(|item| item.push_str(value)),
                    ("TOLOWER", []) => list.iter_mut().for_each(|item| item.make_ascii_lowercase()),
                    ("TOUPPER", []) => list.iter_mut().for_each(|item| item.make_ascii_uppercase()),
                    ("STRIP", []) => list
                        .iter_mut()
                        .for_each(|item| *item = item.trim().to_owned()),
                    ("REPLACE", [regex, replacement]) => {
                        let regex = cmake_regex(regex)?;
                        for item in &mut list {
                            *item = regex_replace(&regex, replacement, item, self)?;
                        }
                    }
                    _ => return fail(format!("unsupported list(TRANSFORM) arguments {args:?}")),
                }
                self.set(output, join(&list));
                return Ok(());
            }
            "REMOVE_ITEM" => list.retain(|item| !args[2..].contains(item)),
            "REMOVE_DUPLICATES" => {
                let mut seen = std::collections::BTreeSet::new();
                list.retain(|item| seen.insert(item.clone()));
            }
            "LENGTH" => {
                need(args, 3)?;
                self.set(&args[2], list.len().to_string());
                return Ok(());
            }
            "FIND" => {
                need(args, 4)?;
                let position = list
                    .iter()
                    .position(|item| item == &args[2])
                    .map_or("-1".to_owned(), |value| value.to_string());
                self.set(&args[3], position);
                return Ok(());
            }
            "JOIN" => {
                need(args, 4)?;
                self.set(&args[3], list.join(&args[2]));
                return Ok(());
            }
            "GET" => {
                need(args, 4)?;
                let output = args.last().ok_or("missing list output")?;
                let values = args[2..args.len() - 1]
                    .iter()
                    .map(|index| {
                        list_index(index, list.len(), false).map(|index| list[index].clone())
                    })
                    .collect::<Result<Vec<_>>>()?;
                self.set(output, join(&values));
                return Ok(());
            }
            operation => return fail(format!("unsupported list operation {operation}")),
        }
        self.set(name, join(&list));
        Ok(())
    }

    fn string(&mut self, args: &[String]) -> Result<()> {
        need(args, 2)?;
        let (output, value) = match args[0].as_str() {
            "APPEND" => (
                &args[1],
                self.get(&args[1]).unwrap_or_default() + &args[2..].concat(),
            ),
            "PREPEND" => (
                &args[1],
                args[2..].concat() + &self.get(&args[1]).unwrap_or_default(),
            ),
            "CONCAT" => (&args[1], args[2..].concat()),
            "TOUPPER" | "TOLOWER" | "STRIP" | "LENGTH" => {
                need(args, 3)?;
                let value = match args[0].as_str() {
                    "TOUPPER" => args[1].to_ascii_uppercase(),
                    "TOLOWER" => args[1].to_ascii_lowercase(),
                    "STRIP" => args[1].trim().to_owned(),
                    _ => args[1].len().to_string(),
                };
                (&args[2], value)
            }
            "MD5" => {
                use md5::{Digest, Md5};
                need(args, 3)?;
                if args.len() != 3 {
                    return fail("string(MD5) expects an output and input string");
                }
                (&args[1], format!("{:x}", Md5::digest(args[2].as_bytes())))
            }
            "REPEAT" => {
                if args.len() != 4 {
                    return fail("string(REPEAT) expects an input, count, and output variable");
                }
                let count = args[2].parse::<usize>()?;
                let size = args[1]
                    .len()
                    .checked_mul(count)
                    .ok_or("string(REPEAT) size overflow")?;
                let mut value = String::new();
                value.try_reserve(size)?;
                for _ in 0..count {
                    value.push_str(&args[1]);
                }
                (&args[3], value)
            }
            "REPLACE" => {
                need(args, 5)?;
                (&args[3], args[4..].concat().replace(&args[1], &args[2]))
            }
            "REGEX" => {
                need(args, 5)?;
                let regex = cmake_regex(&args[2])?;
                match args[1].as_str() {
                    "MATCH" | "MATCHALL" => {
                        let input = args[4..].concat();
                        let mut matches = regex.captures_iter(&input);
                        if args[1] == "MATCH" {
                            let capture = matches.next();
                            self.save_matches(capture.as_ref());
                            (
                                &args[3],
                                capture.map_or(String::new(), |group| group[0].to_owned()),
                            )
                        } else {
                            let mut all = Vec::new();
                            self.save_matches(None);
                            for group in matches {
                                all.push(group[0].to_owned());
                                self.save_matches(Some(&group));
                            }
                            (&args[3], all.join(";"))
                        }
                    }
                    "REPLACE" => {
                        need(args, 6)?;
                        let input = args[5..].concat();
                        let value = regex_replace(&regex, &args[3], &input, self)?;
                        (&args[4], value)
                    }
                    operation => return fail(format!("unsupported string(REGEX {operation})")),
                }
            }
            operation => return fail(format!("unsupported string operation {operation}")),
        };
        self.set(output, value);
        Ok(())
    }

    pub(super) fn save_matches(&mut self, captures: Option<&Captures<'_>>) {
        for index in 0..10 {
            self.variables_mut().remove(&format!("CMAKE_MATCH_{index}"));
        }
        let count = captures.map_or(0, |groups| groups.len().saturating_sub(1).min(9));
        self.set("CMAKE_MATCH_COUNT", count.to_string());
        if let Some(groups) = captures {
            for index in 0..=count {
                if let Some(group) = groups.get(index) {
                    self.set(format!("CMAKE_MATCH_{index}"), group.as_str());
                }
            }
        }
    }

    fn get_cmake_property(&mut self, args: &[String]) -> Result<()> {
        need(args, 2)?;
        let values = match args[1].as_str() {
            "VARIABLES" => self
                .variables()
                .keys()
                .chain(self.cache.keys())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(";"),
            "CACHE_VARIABLES" => self.cache.keys().cloned().collect::<Vec<_>>().join(";"),
            "COMPONENTS" => if self.graph.install.is_empty() {
                ""
            } else {
                "Unspecified"
            }
            .into(),
            property => {
                return fail(format!(
                    "unsupported get_cmake_property property {property}"
                ));
            }
        };
        self.set(&args[0], values);
        Ok(())
    }

    fn get_property(&mut self, args: &[String]) -> Result<()> {
        need(args, 4)?;
        let (output, scope) = (&args[0], args[1].as_str());
        let (value, query) = match (scope, &args[2..]) {
            ("GLOBAL", [marker, name, query @ ..]) if marker == "PROPERTY" => {
                let value = match name.as_str() {
                    "GENERATOR_IS_MULTI_CONFIG" => Some("0"),
                    "CMAKE_ROLE" if self.get("CMAKE_SCRIPT_MODE_FILE").is_some() => Some("SCRIPT"),
                    "CMAKE_ROLE" => Some("PROJECT"),
                    _ => self.global_properties.get(name).map(String::as_str),
                };
                (value, query)
            }
            ("DIRECTORY", [marker, name, query @ ..]) if marker == "PROPERTY" => (
                self.directory().properties.get(name).map(String::as_str),
                query,
            ),
            ("CACHE", [entry, marker, name, query @ ..]) if marker == "PROPERTY" => (
                self.cache.get(entry).and_then(|entry| entry.property(name)),
                query,
            ),
            _ => return fail(format!("unsupported get_property arguments {args:?}")),
        };
        match query {
            [] => {
                if let Some(value) = value {
                    self.set(output, value.to_owned());
                } else {
                    self.variables_mut().remove(output);
                }
            }
            [mode] if mode == "SET" => self.set(output, if value.is_some() { "1" } else { "0" }),
            _ => return fail(format!("unsupported get_property query {query:?}")),
        }
        Ok(())
    }

    fn set_property(&mut self, args: &[String]) -> Result<()> {
        need(args, 3)?;
        let marker = args
            .iter()
            .position(|value| value == "PROPERTY")
            .filter(|index| *index != 0)
            .ok_or("expected PROPERTY")?;
        let name = args.get(marker + 1).ok_or("missing property name")?;
        let values = &args[marker + 2..];
        let (scope, mode) = match args[1..marker].last().map(String::as_str) {
            Some(mode @ ("APPEND" | "APPEND_STRING")) => (&args[1..marker - 1], mode),
            _ => (&args[1..marker], ""),
        };
        let mut value = values.join(";");
        let assigned = |old: Option<String>, new: &mut String| {
            if let Some(mut old) = old.filter(|old| !old.is_empty() && !mode.is_empty()) {
                if mode == "APPEND" && !new.is_empty() {
                    old.push(';');
                }
                old.push_str(new);
                *new = old;
            }
        };
        let properties = match (args[0].as_str(), scope) {
            ("GLOBAL", []) => &mut self.global_properties,
            ("DIRECTORY", []) => &mut self.directory_mut().properties,
            ("CACHE", [entry])
                if matches!(
                    name.as_str(),
                    "VALUE" | "TYPE" | "HELPSTRING" | "ADVANCED" | "STRINGS"
                ) =>
            {
                let Some(existing) = self.cache.get_mut(entry) else {
                    return fail(format!("no cache entry {entry}"));
                };
                assigned(existing.property(name).map(str::to_owned), &mut value);
                existing.set_property(name, value);
                return Ok(());
            }
            _ => return fail(format!("unsupported set_property scope {}", args[0])),
        };
        assigned(properties.get(name).cloned(), &mut value);
        properties.insert(name.clone(), value);
        Ok(())
    }

    fn set_directory_properties(&mut self, args: &[String]) -> Result<()> {
        if args.first().map(String::as_str) != Some("PROPERTIES") || args.len() % 2 != 1 {
            return fail("expected PROPERTIES followed by key/value pairs");
        }
        for pair in args[1..].chunks_exact(2) {
            self.directory_mut()
                .properties
                .insert(pair[0].clone(), pair[1].clone());
        }
        Ok(())
    }

    fn get_filename_component(&mut self, args: &[String]) -> Result<()> {
        need(args, 3)?;
        let path = Path::new(&args[1]);
        let value = match args[2].as_str() {
            "DIRECTORY" | "PATH" => path.parent().map_or_else(String::new, path_string),
            "NAME" => path
                .file_name()
                .map_or_else(String::new, |value| value.to_string_lossy().into_owned()),
            "NAME_WE" | "NAME_WLE" => {
                let name = path
                    .file_name()
                    .ok_or("path has no filename")?
                    .to_string_lossy();
                if args[2] == "NAME_WE" {
                    name.split_once('.')
                        .map_or(name.as_ref(), |(stem, _)| stem)
                        .to_owned()
                } else {
                    path.file_stem()
                        .map_or_else(String::new, |value| value.to_string_lossy().into_owned())
                }
            }
            "ABSOLUTE" | "REALPATH" => {
                let base = match &args[3..] {
                    [key, base] if key == "BASE_DIR" => Path::new(base),
                    [] => &self.directory().source,
                    _ => return fail("unsupported get_filename_component options"),
                };
                let absolute = base.join(path);
                if args[2] == "REALPATH" {
                    path_string(&absolute.canonicalize().unwrap_or(absolute))
                } else {
                    path_string(&normalize(&absolute))
                }
            }
            mode => return fail(format!("unsupported path component {mode}")),
        };
        self.set(&args[0], value);
        Ok(())
    }
}

fn regex_replace(
    regex: &Regex,
    replacement: &str,
    input: &str,
    state: &mut Interpreter,
) -> Result<String> {
    let mut value = String::new();
    let mut end = 0;
    state.save_matches(None);
    for group in regex.captures_iter(input) {
        let matched = group.get(0).ok_or("regex match missing group zero")?;
        value.push_str(&input[end..matched.start()]);
        value.push_str(&replace_match(replacement, &group)?);
        state.save_matches(Some(&group));
        end = matched.end();
    }
    value.push_str(&input[end..]);
    Ok(value)
}

fn replace_match(replacement: &str, group: &Captures<'_>) -> Result<String> {
    let mut result = String::new();
    let mut chars = replacement.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('\\') => result.push('\\'),
                Some(number) if number.is_ascii_digit() => {
                    let index = number.to_digit(10).ok_or("invalid regex group number")? as usize;
                    if let Some(value) = group.get(index) {
                        result.push_str(value.as_str());
                    } else if index >= group.len() {
                        return fail(format!("invalid regex replacement group {index}"));
                    }
                }
                _ => return fail("unsupported regex replacement escape"),
            }
        } else {
            result.push(ch);
        }
    }
    Ok(result)
}

fn list_index(index: &str, length: usize, allow_end: bool) -> Result<usize> {
    let length = isize::try_from(length)?;
    let offset: isize = index.parse()?;
    let offset = if offset < 0 { length + offset } else { offset };
    if offset < 0 || offset > length || (offset == length && !allow_end) {
        return fail(format!(
            "list index {index} out of bounds for length {length}"
        ));
    }
    Ok(usize::try_from(offset)?)
}

fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut normalized = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir if normalized.file_name().is_some() => {
                normalized.pop();
            }
            Component::ParentDir if normalized.has_root() => {}
            Component::CurDir => {}
            _ => normalized.push(part),
        }
    }
    normalized
}
