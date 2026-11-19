//! Commands that populate the build graph: targets, tests, and install rules.

use std::collections::BTreeMap;
use std::path::Path;

use super::{Interpreter, is_false, need};
use crate::graph::{Install, Kind, Setting, Target, Test, Visibility, shell_words};
use crate::syntax::{self, Location};
use crate::{Result, fail};

/// Commands handled by [`Interpreter::graph_command`].
const COMMANDS: &[&str] = &[
    "add_definitions",
    "add_executable",
    "add_library",
    "add_test",
    "enable_testing",
    "include_directories",
    "install",
    "set_target_properties",
    "set_tests_properties",
    "target_compile_definitions",
    "target_compile_options",
    "target_include_directories",
    "target_link_libraries",
    "target_link_options",
];

/// install(TARGETS) artifact groups that carry their own DESTINATION.
const INSTALL_GROUPS: [&str; 3] = ["RUNTIME", "ARCHIVE", "LIBRARY"];

/// install() keywords whose semantics cannot be reproduced faithfully.
const INSTALL_UNSUPPORTED: &[&str] = &[
    "BUNDLE",
    "COMPONENT",
    "CONFIGURATIONS",
    "DIRECTORY_PERMISSIONS",
    "EXCLUDE_FROM_ALL",
    "EXPORT",
    "FILE_PERMISSIONS",
    "FILE_SET",
    "FILES_MATCHING",
    "FRAMEWORK",
    "INCLUDES",
    "MESSAGE_NEVER",
    "NAMELINK_COMPONENT",
    "NAMELINK_ONLY",
    "NAMELINK_SKIP",
    "OBJECTS",
    "OPTIONAL",
    "PATTERN",
    "PERMISSIONS",
    "PRIVATE_HEADER",
    "PUBLIC_HEADER",
    "REGEX",
    "RENAME",
    "RESOURCE",
    "RUNTIME_DEPENDENCIES",
    "TYPE",
    "USE_SOURCE_PERMISSIONS",
];

impl Interpreter {
    /// Reports whether `name` is a command handled by [`graph_command`](Self::graph_command).
    pub(super) fn knows_graph_command(&self, name: &str) -> bool {
        COMMANDS.contains(&name)
    }

    /// Runs a graph command. Returns `false` when `command` is not handled here
    /// so the caller can continue dispatching.
    pub(super) fn graph_command(
        &mut self,
        command: &syntax::Command,
        args: &[String],
    ) -> Result<bool> {
        match command.name.as_str() {
            "add_library" => self.add_library(command, args)?,
            "add_executable" => self.add_executable(command, args)?,
            "set_target_properties" => self.set_target_properties(command, args)?,
            "target_link_libraries" => self.target_link_libraries(command, args)?,
            "target_include_directories" => self.target_include_directories(command, args)?,
            "target_compile_definitions" => self.target_compile_definitions(command, args)?,
            "target_compile_options" => self.target_compile_options(command, args)?,
            "target_link_options" => self.target_link_options(command, args)?,
            "add_test" => self.add_test(command, args)?,
            "enable_testing" => self.enable_testing(args)?,
            "set_tests_properties" => self.set_tests_properties(command, args)?,
            "install" => self.install(command, args)?,
            "include_directories" => self.include_directories(command, args)?,
            "add_definitions" => self.add_definitions(command, args)?,
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// `add_library(<name> [STATIC|SHARED|INTERFACE] [EXCLUDE_FROM_ALL] [sources...])`
    /// and `add_library(<name> ALIAS <target>)`. With no library type the
    /// BUILD_SHARED_LIBS variable selects between STATIC and SHARED.
    fn add_library(&mut self, command: &syntax::Command, args: &[String]) -> Result<()> {
        need(args, 1)?;
        let name = &args[0];
        if args.get(1).is_some_and(|arg| arg == "ALIAS") {
            if args.len() != 3 {
                return fail("add_library(ALIAS) expects a name and a single target");
            }
            return self.add_alias(name, &args[2]);
        }
        let mut kind = None;
        let mut excluded = false;
        let mut sources = Vec::new();
        for arg in &args[1..] {
            match arg.as_str() {
                "STATIC" | "SHARED" | "INTERFACE" if sources.is_empty() && kind.is_none() => {
                    kind = Some(match arg.as_str() {
                        "STATIC" => Kind::Static,
                        "SHARED" => Kind::Shared,
                        _ => Kind::Interface,
                    });
                }
                "MODULE" | "OBJECT" | "UNKNOWN" if sources.is_empty() && kind.is_none() => {
                    return fail(format!("add_library type {arg} is not supported"));
                }
                "EXCLUDE_FROM_ALL" if sources.is_empty() => excluded = true,
                "IMPORTED" | "GLOBAL" => return fail("imported libraries are not supported"),
                _ => {
                    let source = self.genex(arg, &command.location)?;
                    if !source.is_empty() {
                        sources.push(source);
                    }
                }
            }
        }
        let kind = match kind {
            Some(kind) => kind,
            None if self
                .get("BUILD_SHARED_LIBS")
                .is_some_and(|value| !is_false(&value)) =>
            {
                Kind::Shared
            }
            None => Kind::Static,
        };
        if kind == Kind::Interface && !sources.is_empty() {
            return fail("INTERFACE libraries cannot have sources");
        }
        self.add_target(name, kind, sources, excluded)
    }

    /// `add_executable(<name> [EXCLUDE_FROM_ALL] [sources...])` and
    /// `add_executable(<name> ALIAS <target>)`.
    fn add_executable(&mut self, command: &syntax::Command, args: &[String]) -> Result<()> {
        need(args, 1)?;
        let name = &args[0];
        if args.get(1).is_some_and(|arg| arg == "ALIAS") {
            if args.len() != 3 {
                return fail("add_executable(ALIAS) expects a name and a single target");
            }
            return self.add_alias(name, &args[2]);
        }
        let mut excluded = false;
        let mut sources = Vec::new();
        for arg in &args[1..] {
            match arg.as_str() {
                "EXCLUDE_FROM_ALL" if sources.is_empty() => excluded = true,
                "WIN32" | "MACOSX_BUNDLE" => {
                    return fail(format!("add_executable({arg}) bundles are not supported"));
                }
                _ => {
                    let source = self.genex(arg, &command.location)?;
                    if !source.is_empty() {
                        sources.push(source);
                    }
                }
            }
        }
        self.add_target(name, Kind::Executable, sources, excluded)
    }

    /// `set_target_properties(<target>... PROPERTIES <key> <value>...)`.
    fn set_target_properties(&mut self, command: &syntax::Command, args: &[String]) -> Result<()> {
        need(args, 1)?;
        let Some(index) = args.iter().position(|arg| arg == "PROPERTIES") else {
            return fail("set_target_properties expects PROPERTIES");
        };
        if index == 0 {
            return fail("set_target_properties expects at least one target");
        }
        let pairs = &args[index + 1..];
        if pairs.len() % 2 != 0 {
            return fail("set_target_properties expects property/value pairs");
        }
        let mut properties = Vec::new();
        for pair in pairs.chunks_exact(2) {
            properties.push((pair[0].clone(), self.genex(&pair[1], &command.location)?));
        }
        for name in &args[..index] {
            let target = self.target_mut(name)?;
            for (key, value) in &properties {
                match key.as_str() {
                    "C_STANDARD" => {
                        target.standards[0] = (!value.is_empty()).then(|| value.clone())
                    }
                    "CXX_STANDARD" => {
                        target.standards[1] = (!value.is_empty()).then(|| value.clone())
                    }
                    "C_EXTENSIONS" => target.extensions[0] = !is_false(value),
                    "CXX_EXTENSIONS" => target.extensions[1] = !is_false(value),
                    _ => {}
                }
                target
                    .properties
                    .insert(key.clone(), (value.clone(), command.location.clone()));
            }
        }
        Ok(())
    }

    /// `target_link_libraries(<target> [item | PRIVATE|PUBLIC|INTERFACE item...]...)`.
    /// The plain signature makes items public usage requirements.
    fn target_link_libraries(&mut self, command: &syntax::Command, args: &[String]) -> Result<()> {
        need(args, 1)?;
        for arg in &args[1..] {
            match arg.as_str() {
                "debug" | "optimized" | "general" => {
                    return fail(format!(
                        "target_link_libraries {arg} keyword is not supported"
                    ));
                }
                "LINK_PRIVATE" | "LINK_PUBLIC" => {
                    return fail(
                        "target_link_libraries LINK_PRIVATE/LINK_PUBLIC are not supported",
                    );
                }
                _ => {}
            }
        }
        let (_, settings) = self.scoped(&command.location, &args[1..], Visibility::Public)?;
        let target = self.target_mut(&args[0])?;
        require_interface_scope(&args[0], target, &settings)?;
        target.links.extend(settings);
        Ok(())
    }

    /// `target_include_directories(<target> [SYSTEM] [AFTER|BEFORE]
    /// [item | PRIVATE|PUBLIC|INTERFACE item...]...)`. Relative paths resolve
    /// against the current source directory.
    fn target_include_directories(
        &mut self,
        command: &syntax::Command,
        args: &[String],
    ) -> Result<()> {
        need(args, 1)?;
        let mut before = false;
        let mut rest = &args[1..];
        while let Some(first) = rest.first().map(String::as_str) {
            match first {
                "SYSTEM" => return fail("target_include_directories(SYSTEM) is not supported"),
                "BEFORE" => {
                    before = true;
                    rest = &rest[1..];
                }
                "AFTER" => rest = &rest[1..],
                _ => break,
            }
        }
        let (later, mut settings) = self.scoped(&command.location, rest, Visibility::Private)?;
        before |= later;
        for setting in &mut settings {
            setting.value = self.absolute(&setting.value);
        }
        let target = self.target_mut(&args[0])?;
        require_interface_scope(&args[0], target, &settings)?;
        put(&mut target.includes, settings, before);
        Ok(())
    }

    /// `target_compile_definitions(<target> [item | PRIVATE|PUBLIC|INTERFACE item...]...)`.
    /// A leading -D is stripped from each item.
    fn target_compile_definitions(
        &mut self,
        command: &syntax::Command,
        args: &[String],
    ) -> Result<()> {
        need(args, 1)?;
        let (before, mut settings) =
            self.scoped(&command.location, &args[1..], Visibility::Private)?;
        settings.retain_mut(|setting| {
            setting.value = setting
                .value
                .strip_prefix("-D")
                .unwrap_or(&setting.value)
                .to_owned();
            !setting.value.is_empty()
        });
        let target = self.target_mut(&args[0])?;
        require_interface_scope(&args[0], target, &settings)?;
        put(&mut target.definitions, settings, before);
        Ok(())
    }

    /// `target_compile_options(<target> [BEFORE] [item | PRIVATE|PUBLIC|INTERFACE item...]...)`.
    fn target_compile_options(&mut self, command: &syntax::Command, args: &[String]) -> Result<()> {
        need(args, 1)?;
        let (before, settings) = self.scoped(&command.location, &args[1..], Visibility::Private)?;
        let target = self.target_mut(&args[0])?;
        require_interface_scope(&args[0], target, &settings)?;
        put(&mut target.options, settings, before);
        Ok(())
    }

    /// `target_link_options(<target> [BEFORE] [item | PRIVATE|PUBLIC|INTERFACE item...]...)`.
    fn target_link_options(&mut self, command: &syntax::Command, args: &[String]) -> Result<()> {
        need(args, 1)?;
        let (before, settings) = self.scoped(&command.location, &args[1..], Visibility::Private)?;
        let target = self.target_mut(&args[0])?;
        require_interface_scope(&args[0], target, &settings)?;
        put(&mut target.link_options, settings, before);
        Ok(())
    }

    /// `add_test(<name> <command> [args...])` and
    /// `add_test(NAME <name> COMMAND <command> [args...] [WORKING_DIRECTORY <dir>])`.
    fn add_test(&mut self, command: &syntax::Command, args: &[String]) -> Result<()> {
        need(args, 2)?;
        let (name, rest, keywords) = if args[0] == "NAME" {
            if args.len() < 4 || args[2] != "COMMAND" {
                return fail("add_test expects NAME <name> COMMAND <command> [args...]");
            }
            (args[1].clone(), &args[3..], true)
        } else {
            (args[0].clone(), &args[1..], false)
        };
        let mut test_args = Vec::new();
        let mut properties = BTreeMap::new();
        let mut index = 0;
        while index < rest.len() {
            if keywords && rest[index] == "WORKING_DIRECTORY" {
                index += 1;
                let Some(directory) = rest.get(index) else {
                    return fail("add_test WORKING_DIRECTORY expects a directory");
                };
                properties.insert(
                    "WORKING_DIRECTORY".to_owned(),
                    self.genex(directory, &command.location)?,
                );
            } else if keywords && rest[index] == "COMMAND_EXPAND_LISTS" {
                return fail("add_test COMMAND_EXPAND_LISTS is not supported");
            } else {
                let arg = self.genex(&rest[index], &command.location)?;
                if !arg.is_empty() {
                    test_args.push(arg);
                }
            }
            index += 1;
        }
        if test_args.is_empty() {
            return fail("add_test expects a command");
        }
        let directory = self.directory().binary.clone();
        self.graph.directories.insert(directory.clone());
        self.graph.tests.push(Test {
            name,
            args: test_args,
            directory,
            properties,
        });
        Ok(())
    }

    /// `enable_testing()` enables CTest generation for the whole build tree.
    fn enable_testing(&mut self, args: &[String]) -> Result<()> {
        if !args.is_empty() {
            return fail("enable_testing expects no arguments");
        }
        self.graph.testing = true;
        let binary = self.directory().binary.clone();
        self.graph.directories.insert(binary);
        Ok(())
    }

    /// `set_tests_properties(<test>... PROPERTIES <key> <value>...)`.
    fn set_tests_properties(&mut self, _command: &syntax::Command, args: &[String]) -> Result<()> {
        need(args, 1)?;
        let Some(index) = args.iter().position(|arg| arg == "PROPERTIES") else {
            return fail("set_tests_properties expects PROPERTIES");
        };
        if index == 0 {
            return fail("set_tests_properties expects at least one test");
        }
        let pairs = &args[index + 1..];
        if pairs.len() % 2 != 0 {
            return fail("set_tests_properties expects property/value pairs");
        }
        for name in &args[..index] {
            let Some(test) = self.graph.tests.iter_mut().find(|test| &test.name == name) else {
                return fail(format!("unknown test {name}"));
            };
            for pair in pairs.chunks_exact(2) {
                test.properties.insert(pair[0].clone(), pair[1].clone());
            }
        }
        Ok(())
    }

    /// `install(FILES <file>... DESTINATION <dir>)` and
    /// `install(TARGETS <target>... [DESTINATION <dir>]
    /// [RUNTIME|ARCHIVE|LIBRARY DESTINATION <dir>]...)`.
    fn install(&mut self, command: &syntax::Command, args: &[String]) -> Result<()> {
        need(args, 1)?;
        match args[0].as_str() {
            "FILES" => self.install_files(command, args),
            "TARGETS" => self.install_targets(command, args),
            mode => fail(format!("install({mode}) is not supported")),
        }
    }

    /// `include_directories([AFTER|BEFORE] <dir>...)`. Relative paths resolve
    /// against the current source directory and apply to targets created later
    /// in this directory.
    fn include_directories(&mut self, command: &syntax::Command, args: &[String]) -> Result<()> {
        need(args, 1)?;
        let mut before = false;
        let mut index = 0;
        for arg in args {
            match arg.as_str() {
                "BEFORE" | "AFTER" if index == 0 => before = arg == "BEFORE",
                "SYSTEM" => return fail("include_directories(SYSTEM) is not supported"),
                _ => break,
            }
            index += 1;
        }
        let mut settings = Vec::new();
        for arg in &args[index..] {
            let value = self.genex(arg, &command.location)?;
            if value.is_empty() {
                continue;
            }
            settings.push(Setting {
                value: self.absolute(&value),
                origin: command.location.clone(),
                visibility: Visibility::Private,
            });
        }
        put(&mut self.directory_mut().includes, settings, before);
        Ok(())
    }

    /// `add_definitions(-D<name>[=value]...)`. Only -D flags are representable
    /// as compile definitions; anything else is rejected explicitly.
    fn add_definitions(&mut self, command: &syntax::Command, args: &[String]) -> Result<()> {
        for arg in args {
            let value = self.genex(arg, &command.location)?;
            let Some(definition) = value.strip_prefix("-D") else {
                return fail(format!("add_definitions value {value:?} is not a -D flag"));
            };
            if definition.is_empty() {
                continue;
            }
            self.directory_mut().definitions.push(Setting {
                value: definition.to_owned(),
                origin: command.location.clone(),
                visibility: Visibility::Private,
            });
        }
        Ok(())
    }

    /// Creates a target in the current directory, inheriting the directory
    /// include directories and compile definitions and snapshotting the active
    /// CMAKE_<LANG>_FLAGS(_<CONFIG>) and standard settings.
    fn add_target(
        &mut self,
        name: &str,
        kind: Kind,
        sources: Vec<String>,
        excluded: bool,
    ) -> Result<()> {
        if name.contains("::") {
            return fail(format!("target name {name:?} is reserved"));
        }
        if self.graph.targets.contains_key(name) || self.graph.aliases.contains_key(name) {
            return fail(format!("target {name} is already defined"));
        }
        let (source, binary, includes, definitions) = {
            let directory = self.directory();
            (
                directory.source.clone(),
                directory.binary.clone(),
                directory.includes.clone(),
                directory.definitions.clone(),
            )
        };
        let c_flags = self.flags_for("C")?;
        let cxx_flags = self.flags_for("CXX")?;
        let standards = [self.standard("C"), self.standard("CXX")];
        let extensions = [self.extensions("C"), self.extensions("CXX")];
        self.graph.targets.insert(
            name.to_owned(),
            Target {
                kind,
                source,
                binary,
                sources,
                includes,
                definitions,
                options: Vec::new(),
                links: Vec::new(),
                link_options: Vec::new(),
                properties: BTreeMap::new(),
                c_flags,
                cxx_flags,
                standards,
                extensions,
                excluded,
            },
        );
        Ok(())
    }

    /// `add_library(<name> ALIAS <target>)` / `add_executable(<name> ALIAS <target>)`.
    fn add_alias(&mut self, name: &str, target: &str) -> Result<()> {
        let mut resolved = target;
        while let Some(next) = self.graph.aliases.get(resolved) {
            resolved = next;
        }
        if !self.graph.targets.contains_key(resolved) {
            return fail(format!("cannot alias {name}: unknown target {target}"));
        }
        if self.graph.targets.contains_key(name) || self.graph.aliases.contains_key(name) {
            return fail(format!("target {name} is already defined"));
        }
        self.graph
            .aliases
            .insert(name.to_owned(), resolved.to_owned());
        Ok(())
    }

    fn target_mut(&mut self, name: &str) -> Result<&mut Target> {
        let resolved = self
            .graph
            .aliases
            .get(name)
            .map_or(name, String::as_str)
            .to_owned();
        self.graph
            .targets
            .get_mut(&resolved)
            .ok_or_else(|| format!("unknown target {name}").into())
    }

    /// `install(FILES <file>... DESTINATION <dir>)`.
    fn install_files(&mut self, command: &syntax::Command, args: &[String]) -> Result<()> {
        let mut files = Vec::new();
        let mut destination = None;
        let mut index = 1;
        while index < args.len() {
            match args[index].as_str() {
                "DESTINATION" => {
                    index += 1;
                    let Some(dir) = args.get(index) else {
                        return fail("install DESTINATION expects a directory");
                    };
                    if destination.is_some() {
                        return fail("install(FILES): duplicate DESTINATION");
                    }
                    destination = Some(self.genex(dir, &command.location)?);
                }
                keyword if INSTALL_UNSUPPORTED.contains(&keyword) => {
                    return fail(format!("install(FILES) keyword {keyword} is not supported"));
                }
                file if destination.is_none() => {
                    let value = self.genex(file, &command.location)?;
                    if !value.is_empty() {
                        files.push(value);
                    }
                }
                other => return fail(format!("install(FILES): unexpected argument {other:?}")),
            }
            index += 1;
        }
        if files.is_empty() {
            return fail("install(FILES) expects at least one file");
        }
        let Some(destination) = destination else {
            return fail("install(FILES) expects a DESTINATION");
        };
        for file in files {
            let path = Path::new(&file);
            let source = if path.is_absolute() {
                path.to_owned()
            } else {
                self.directory().source.join(path)
            };
            self.graph.install.push(Install::File {
                source,
                destination: destination.clone(),
            });
        }
        Ok(())
    }

    /// `install(TARGETS <target>... [DESTINATION <dir>] [RUNTIME|ARCHIVE|LIBRARY
    /// DESTINATION <dir>]...)`. Each target is installed to the destination of
    /// its artifact group, falling back to a shared DESTINATION and then to the
    /// usual bin/lib defaults.
    fn install_targets(&mut self, command: &syntax::Command, args: &[String]) -> Result<()> {
        let mut names = Vec::new();
        let mut index = 1;
        while index < args.len() && !install_keyword(&args[index]) {
            names.push(args[index].clone());
            index += 1;
        }
        if names.is_empty() {
            return fail("install(TARGETS) expects at least one target");
        }
        let mut destinations: [Option<String>; 3] = [None, None, None];
        let mut shared = None;
        while index < args.len() {
            let word = &args[index];
            if let Some(slot) = INSTALL_GROUPS.iter().position(|group| group == word) {
                index += 1;
                loop {
                    match args.get(index).map(String::as_str) {
                        Some("DESTINATION") => {
                            let Some(dir) = args.get(index + 1) else {
                                return fail("install DESTINATION expects a directory");
                            };
                            if destinations[slot].is_some() {
                                return fail(format!(
                                    "install(TARGETS) {word}: duplicate DESTINATION"
                                ));
                            }
                            destinations[slot] = Some(self.genex(dir, &command.location)?);
                            index += 2;
                        }
                        Some(keyword) if install_keyword(keyword) => break,
                        Some(other) => {
                            return fail(format!(
                                "install(TARGETS) {word} keyword {other} is not supported"
                            ));
                        }
                        None => break,
                    }
                }
            } else if word == "DESTINATION" {
                index += 1;
                let Some(dir) = args.get(index) else {
                    return fail("install DESTINATION expects a directory");
                };
                if shared.is_some() {
                    return fail("install(TARGETS): duplicate DESTINATION");
                }
                shared = Some(self.genex(dir, &command.location)?);
                index += 1;
            } else if INSTALL_UNSUPPORTED.contains(&word.as_str()) {
                return fail(format!("install(TARGETS) keyword {word} is not supported"));
            } else {
                return fail(format!("install(TARGETS): unexpected argument {word:?}"));
            }
        }
        for name in names {
            let name = self.genex(&name, &command.location)?;
            let resolved = self
                .graph
                .aliases
                .get(&name)
                .map_or(name.as_str(), String::as_str)
                .to_owned();
            let Some(target) = self.graph.targets.get(&resolved) else {
                return fail(format!("install(TARGETS): unknown target {name}"));
            };
            let slot = match target.kind {
                Kind::Executable => 0,
                Kind::Static => 1,
                Kind::Shared => 2,
                Kind::Interface => {
                    return fail(format!("install(TARGETS): {name} is an INTERFACE library"));
                }
            };
            let destination = destinations[slot]
                .clone()
                .or_else(|| shared.clone())
                .unwrap_or_else(|| ["bin", "lib", "lib"][slot].to_owned());
            self.graph.install.push(Install::Target {
                name: resolved,
                destination,
            });
        }
        Ok(())
    }

    /// Parses `[AFTER|BEFORE]` followed by scope keywords (PRIVATE/PUBLIC/
    /// INTERFACE) and items into visibility-tagged settings. Items before the
    /// first scope keyword use `default` visibility, matching the plain
    /// signatures CMake allows.
    fn scoped(
        &self,
        location: &Location,
        items: &[String],
        default: Visibility,
    ) -> Result<(bool, Vec<Setting>)> {
        let mut items = items;
        let mut before = false;
        if let Some(first) = items.first().map(String::as_str) {
            if first == "BEFORE" || first == "AFTER" {
                before = first == "BEFORE";
                items = &items[1..];
            }
        }
        let mut visibility = default;
        let mut settings = Vec::new();
        for item in items {
            match item.as_str() {
                "PRIVATE" => visibility = Visibility::Private,
                "PUBLIC" => visibility = Visibility::Public,
                "INTERFACE" => visibility = Visibility::Interface,
                _ => {
                    let value = self.genex(item, location)?;
                    if !value.is_empty() {
                        settings.push(Setting {
                            value,
                            origin: location.clone(),
                            visibility,
                        });
                    }
                }
            }
        }
        Ok((before, settings))
    }

    /// Resolves a path against the current source directory.
    fn absolute(&self, value: &str) -> String {
        let path = Path::new(value);
        if path.is_absolute() {
            value.to_owned()
        } else {
            self.directory().source.join(path).display().to_string()
        }
    }

    /// Compile flags for `language` ("C" or "CXX"): CMAKE_<LANG>_FLAGS plus
    /// CMAKE_<LANG>_FLAGS_<CONFIG> for the current build type, looked up
    /// case-insensitively and defaulting to the usual Debug/Release values.
    fn flags_for(&self, language: &str) -> Result<Vec<String>> {
        let mut flags = Vec::new();
        if let Some(value) = self
            .get(&format!("CMAKE_{language}_FLAGS"))
            .filter(|value| !value.is_empty())
        {
            flags.extend(shell_words(&value)?);
        }
        let config = self.get("CMAKE_BUILD_TYPE").unwrap_or_default();
        if !config.is_empty() {
            let key = format!("CMAKE_{language}_FLAGS_{}", config.to_ascii_uppercase());
            match self
                .get(&key)
                .or_else(|| self.flags_variable(language, &config))
            {
                Some(value) => flags.extend(shell_words(&value)?),
                None => flags.extend(default_flags(&config).iter().map(|flag| flag.to_string())),
            }
        }
        if cfg!(target_os = "macos") {
            let architectures = self
                .get("CMAKE_OSX_ARCHITECTURES")
                .unwrap_or_else(|| self.get("CMAKE_HOST_SYSTEM_PROCESSOR").unwrap_or_default());
            for arch in crate::syntax::split_list(&architectures)
                .into_iter()
                .filter(|arch| !arch.is_empty())
            {
                flags.extend(["-arch".into(), arch]);
            }
            if let Some(sdk) = self.get("CMAKE_OSX_SYSROOT").filter(|sdk| !sdk.is_empty()) {
                flags.extend(["-isysroot".into(), sdk]);
            }
            if let Some(version) = self
                .get("CMAKE_OSX_DEPLOYMENT_TARGET")
                .filter(|version| !version.is_empty())
            {
                flags.push(format!("-mmacosx-version-min={version}"));
            }
        }
        Ok(flags)
    }

    /// Finds a stored CMAKE_<LANG>_FLAGS_<suffix> where <suffix> matches the
    /// build type without regard to case.
    fn flags_variable(&self, language: &str, config: &str) -> Option<String> {
        let prefix = format!("CMAKE_{language}_FLAGS_");
        self.variables()
            .iter()
            .chain(self.cache.iter().map(|(name, entry)| (name, &entry.value)))
            .find(|(name, _)| {
                name.strip_prefix(&prefix)
                    .is_some_and(|suffix| suffix.eq_ignore_ascii_case(config))
            })
            .map(|(_, value)| value.clone())
    }

    fn standard(&self, language: &str) -> Option<String> {
        self.get(&format!("CMAKE_{language}_STANDARD"))
            .filter(|value| !value.is_empty())
    }

    fn extensions(&self, language: &str) -> bool {
        self.get(&format!("CMAKE_{language}_EXTENSIONS"))
            .is_none_or(|value| !is_false(&value))
    }

    /// Evaluates `$<...>` generator expressions in `text`, including nested
    /// expressions. Build-time interfaces are selected; install-time ones
    /// expand empty. Unknown expressions are errors.
    fn genex(&self, text: &str, location: &Location) -> Result<String> {
        let mut result = String::new();
        let mut rest = text;
        while let Some(start) = rest.find("$<") {
            result.push_str(&rest[..start]);
            let inner_start = start + 2;
            let Some(end) = genex_close(&rest[inner_start..]) else {
                return fail(format!(
                    "{location}: unclosed generator expression in {text:?}"
                ));
            };
            result.push_str(&self.genex_inner(&rest[inner_start..inner_start + end], location)?);
            rest = &rest[inner_start + end + 1..];
        }
        result.push_str(rest);
        Ok(result)
    }

    fn genex_inner(&self, inner: &str, location: &Location) -> Result<String> {
        let expanded = self.genex(inner, location)?;
        let (head, tail) = expanded.split_once(':').unwrap_or((expanded.as_str(), ""));
        Ok(match head {
            "BOOL" => truthy(genex_bool(tail)),
            "NOT" => truthy(!genex_bool(tail)),
            "AND" => truthy(split_top(tail, ',').iter().all(|arg| genex_bool(arg))),
            "OR" => truthy(split_top(tail, ',').iter().any(|arg| genex_bool(arg))),
            "IF" => {
                let parts = split_top(tail, ',');
                if parts.len() != 3 {
                    return fail(format!("{location}: $<IF:...> expects three arguments"));
                }
                parts[usize::from(!genex_bool(&parts[0])) + 1].clone()
            }
            "BUILD_INTERFACE" => tail.to_owned(),
            "INSTALL_INTERFACE" => String::new(),
            "CONFIG" | "CONFIGURATION" => {
                let config = self.get("CMAKE_BUILD_TYPE").unwrap_or_default();
                if tail.is_empty() {
                    config
                } else {
                    truthy(
                        split_top(tail, ',')
                            .iter()
                            .any(|name| name.eq_ignore_ascii_case(&config)),
                    )
                }
            }
            "1" => tail.to_owned(),
            "0" => String::new(),
            _ => {
                return fail(format!(
                    "{location}: unsupported generator expression \"$<{head}>\""
                ));
            }
        })
    }
}

/// Requires INTERFACE visibility for settings applied to an interface library.
fn require_interface_scope(name: &str, target: &Target, settings: &[Setting]) -> Result<()> {
    if target.kind == Kind::Interface
        && settings
            .iter()
            .any(|setting| setting.visibility != Visibility::Interface)
    {
        return fail(format!(
            "interface library {name} only accepts INTERFACE items"
        ));
    }
    Ok(())
}

/// Appends `extra` to `settings`, or prepends when `before` is set.
fn put(settings: &mut Vec<Setting>, mut extra: Vec<Setting>, before: bool) {
    if before {
        extra.append(settings);
        *settings = extra;
    } else {
        settings.extend(extra);
    }
}

fn truthy(value: bool) -> String {
    if value {
        "1".to_owned()
    } else {
        "0".to_owned()
    }
}

fn genex_bool(value: &str) -> bool {
    !is_false(value)
}

/// Default flags for the common build types, matching CMake's compiler
/// defaults for GCC-compatible toolchains.
fn default_flags(config: &str) -> &'static [&'static str] {
    match config.to_ascii_uppercase().as_str() {
        "DEBUG" => &["-g"],
        "RELEASE" => &["-O3", "-DNDEBUG"],
        "RELWITHDEBINFO" => &["-O2", "-g", "-DNDEBUG"],
        "MINSIZEREL" => &["-Os", "-DNDEBUG"],
        _ => &[],
    }
}

/// Finds the `>` that closes a generator expression, given the text following
/// its opening `$<`. Returns the byte offset of the closing `>`.
fn genex_close(text: &str) -> Option<usize> {
    let mut depth = 1;
    let mut chars = text.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        if ch == '$' && chars.peek().is_some_and(|&(_, next)| next == '<') {
            chars.next();
            depth += 1;
        } else if ch == '>' {
            depth -= 1;
            if depth == 0 {
                return Some(index);
            }
        }
    }
    None
}

/// Splits `text` on `separator`, ignoring separators nested inside `$<...>`.
fn split_top(text: &str, separator: char) -> Vec<String> {
    let mut parts = Vec::new();
    let mut part = String::new();
    let mut depth = 0;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '$' if chars.peek() == Some(&'<') => {
                chars.next();
                depth += 1;
                part.push('$');
                part.push('<');
            }
            '>' if depth > 0 => {
                depth -= 1;
                part.push(ch);
            }
            ch if ch == separator && depth == 0 => parts.push(std::mem::take(&mut part)),
            _ => part.push(ch),
        }
    }
    parts.push(part);
    parts
}

/// Whether `word` starts a new install() clause rather than naming a target.
fn install_keyword(word: &str) -> bool {
    word == "DESTINATION" || INSTALL_GROUPS.contains(&word) || INSTALL_UNSUPPORTED.contains(&word)
}
