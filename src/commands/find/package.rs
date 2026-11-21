//! Find modules and CMake package configuration files.

mod search;

use std::collections::{BTreeSet, HashSet};
use std::env;
use std::ffi::CString;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use crate::cache::{CacheType, convert_to_unix_slashes};
use crate::interp::{CmdErr, CmdResult, Interp, err};
use crate::messages::MessageType;
use crate::policy::{PolicyStatus, policy_warning};
use crate::util::{
    CompareOp, expand_list, filename_path, is_full_path, is_off_opt, version_compare,
};

use super::common::*;

pub(super) fn register(interp: &mut Interp) {
    interp.register("find_package", find_package);
}

/// Returns the command registered for a dependency-provider method.
pub fn dependency_provider<'a>(interp: &'a Interp, method: &str) -> Option<&'a str> {
    let (command, methods) = interp.ext.dependency_provider.as_ref()?;
    methods
        .iter()
        .any(|candidate| candidate == method)
        .then_some(command)
}

/// Find-package state belonging to one makefile.
#[derive(Debug, Default)]
pub struct PackageState {
    roots: Vec<Vec<String>>,
    /// Enclosing package names and their find-call sequence indexes.
    pub calls: Vec<(String, u32)>,
    next_call: u32,
    warned_roots: HashSet<(usize, String)>,
    /// Imported targets created while reading a GLOBAL package are global.
    pub global: bool,
}

impl PackageState {
    fn push(&mut self, name: String, roots: Vec<String>) {
        self.roots.push(roots);
        self.calls.push((name, self.next_call));
        self.next_call += 1;
    }

    fn pop(&mut self) {
        self.roots.pop();
        let (_, index) = self.calls.pop().expect("active find_package call");
        self.next_call = index + 1;
        if let Some((_, index)) = self.calls.last_mut() {
            *index = self.next_call;
            self.next_call += 1;
        }
    }
}

pub(super) fn root_paths(interp: &Interp) -> impl Iterator<Item = &String> {
    interp
        .ext
        .find_package
        .get(&interp.mf_index())
        .into_iter()
        .flat_map(|state| state.roots.iter().rev().flatten())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Requirement {
    Default,
    Optional,
    Explicit,
    FindVariable,
    PackageVariable,
}

impl Requirement {
    fn required(self) -> bool {
        matches!(
            self,
            Self::Explicit | Self::FindVariable | Self::PackageVariable
        )
    }
}

#[derive(Clone, Copy)]
enum Section {
    None,
    Required,
    Optional,
    Names,
    Paths,
    Hints,
    Suffixes,
    Configs,
}

#[derive(Default)]
struct Version {
    complete: String,
    min: String,
    max: String,
    max_excluded: bool,
    exact: bool,
}

impl Version {
    fn regex() -> &'static Regex {
        static VERSION: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"^([0-9]+(?:\.[0-9]+)*)(?:\.\.\.(<?)([0-9]+(?:\.[0-9]+)*))?$").unwrap()
        });
        &VERSION
    }

    fn parse(&mut self) -> CmdResult {
        if self.complete.is_empty() {
            return Ok(());
        }
        let Some(parts) = Self::regex().captures(&self.complete) else {
            return err("called with invalid version specification.");
        };
        self.min = parts[1].to_owned();
        if let Some(max) = parts.get(3) {
            self.max = max.as_str().to_owned();
            self.max_excluded = &parts[2] == "<";
            let op = if self.max_excluded {
                CompareOp::GreaterEqual
            } else {
                CompareOp::Greater
            };
            if version_compare(op, &self.min, &self.max) {
                return err("specified version range is empty.");
            }
            if self.exact {
                return err("EXACT cannot be specified with a version range.");
            }
        }
        Ok(())
    }

    fn definitions(&self, prefix: &str) -> Vec<(String, String)> {
        let mut defs = version_vars(prefix, &self.min);
        if !self.max.is_empty() {
            defs.extend(version_vars(&format!("{prefix}_MIN"), &self.min));
            defs.extend(version_vars(&format!("{prefix}_MAX"), &self.max));
            defs.extend([
                (format!("{prefix}_RANGE"), self.complete.clone()),
                (format!("{prefix}_RANGE_MIN"), "INCLUDE".into()),
                (
                    format!("{prefix}_RANGE_MAX"),
                    if self.max_excluded {
                        "EXCLUDE"
                    } else {
                        "INCLUDE"
                    }
                    .into(),
                ),
            ]);
        }
        defs
    }
}

struct Package {
    name: String,
    dir: String,
    options: Options,
    quiet: bool,
    bypass: bool,
    global: bool,
    no_policy_scope: bool,
    no_user_registry: bool,
    no_system_registry: bool,
    requirement: Requirement,
    version: Version,
    use_module: bool,
    use_config: bool,
    components: Vec<String>,
    required: BTreeSet<String>,
    optional: BTreeSet<String>,
    names: Vec<String>,
    configs: Vec<String>,
    suffixes: Vec<String>,
    hints: Vec<String>,
    paths: Vec<String>,
    registry_view: Option<String>,
    considered: Vec<(String, String)>,
    selected_version: String,
    debug: Option<String>,
}

impl Package {
    fn parse(interp: &mut Interp, args: &[String]) -> Result<Self, CmdErr> {
        let Some((name, args)) = args.split_first() else {
            return err("called with incorrect number of arguments");
        };
        let mut pkg = Self {
            name: name.clone(),
            dir: format!("{name}_DIR"),
            options: Options::new(interp, "PACKAGE"),
            quiet: false,
            bypass: false,
            global: interp.is_on("CMAKE_FIND_PACKAGE_TARGETS_GLOBAL"),
            no_policy_scope: false,
            no_user_registry: registry_disabled(interp, ""),
            no_system_registry: registry_disabled(interp, "SYSTEM_"),
            requirement: Requirement::Default,
            version: Version::default(),
            use_module: true,
            use_config: true,
            components: Vec::new(),
            required: BTreeSet::new(),
            optional: BTreeSet::new(),
            names: Vec::new(),
            configs: Vec::new(),
            suffixes: vec![String::new()],
            hints: Vec::new(),
            paths: Vec::new(),
            registry_view: None,
            considered: Vec::new(),
            selected_version: String::new(),
            debug: interp.is_on("CMAKE_FIND_DEBUG_MODE").then(String::new),
        };
        let mut section = Section::None;
        let mut config_args = Vec::new();
        let mut module_args = Vec::new();
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let previous = section;
            section = Section::None;
            match arg.as_str() {
                "QUIET" => pkg.quiet = true,
                "BYPASS_PROVIDER" => pkg.bypass = true,
                "EXACT" => pkg.version.exact = true,
                "GLOBAL" => pkg.global = true,
                "MODULE" => module_args.push(arg.as_str()),
                "CONFIG" | "NO_MODULE" | "NO_CMAKE_BUILDS_PATH" => config_args.push(arg.as_str()),
                "REQUIRED" | "OPTIONAL" => {
                    let requirement = if arg == "REQUIRED" {
                        Requirement::Explicit
                    } else {
                        Requirement::Optional
                    };
                    if pkg.requirement != Requirement::Default && pkg.requirement != requirement {
                        return err("cannot be both REQUIRED and OPTIONAL");
                    }
                    pkg.requirement = requirement;
                    section = Section::Required;
                }
                "COMPONENTS" => section = Section::Required,
                "OPTIONAL_COMPONENTS" => section = Section::Optional,
                "NAMES" | "PATHS" | "HINTS" | "PATH_SUFFIXES" | "CONFIGS" => {
                    config_args.push(arg.as_str());
                    section = match arg.as_str() {
                        "NAMES" => Section::Names,
                        "PATHS" => Section::Paths,
                        "HINTS" => Section::Hints,
                        "PATH_SUFFIXES" => Section::Suffixes,
                        _ => Section::Configs,
                    };
                }
                "NO_POLICY_SCOPE" => pkg.no_policy_scope = true,
                "NO_CMAKE_PACKAGE_REGISTRY" => {
                    pkg.no_user_registry = true;
                    config_args.push(arg.as_str());
                }
                "NO_CMAKE_SYSTEM_PACKAGE_REGISTRY" => {
                    pkg.no_system_registry = true;
                    config_args.push(arg.as_str());
                }
                "REGISTRY_VIEW" => {
                    pkg.registry_view = Some(registry_view(args.next())?);
                    section = previous;
                }
                _ if pkg.options.parse(arg) => config_args.push(arg.as_str()),
                _ => {
                    section = previous;
                    match section {
                        Section::Required => {
                            pkg.required.insert(arg.clone());
                            pkg.components.push(arg.clone());
                        }
                        Section::Optional => {
                            pkg.optional.insert(arg.clone());
                            pkg.components.push(arg.clone());
                        }
                        Section::Names => pkg.names.push(arg.clone()),
                        Section::Paths => pkg.paths.push(arg.clone()),
                        Section::Hints => pkg.hints.push(arg.clone()),
                        Section::Suffixes => pkg.suffixes.extend(suffix(arg)),
                        Section::Configs => {
                            if arg.contains([':', '/', '\\']) || !arg.ends_with(".cmake") {
                                return err(format!(
                                    "given CONFIGS option followed by invalid file name \"{arg}\".  The names given must be file names without a path and with a \".cmake\" extension."
                                ));
                            }
                            pkg.configs.push(arg.clone());
                        }
                        Section::None
                            if pkg.version.complete.is_empty()
                                && Version::regex().is_match(arg) =>
                        {
                            pkg.version.complete.clone_from(arg);
                        }
                        Section::None => {
                            return err(format!("called with invalid argument \"{arg}\""));
                        }
                    }
                }
            }
        }
        if pkg.requirement == Requirement::Default && interp.is_on("CMAKE_FIND_REQUIRED") {
            pkg.requirement = Requirement::FindVariable;
        }
        let doubled: Vec<_> = pkg
            .required
            .intersection(&pkg.optional)
            .map(String::as_str)
            .collect();
        if !doubled.is_empty() {
            return err(format!(
                "called with components that are both required and optional:\n  {}\n",
                doubled.join("\n  ")
            ));
        }
        pkg.use_module = config_args.is_empty();
        pkg.use_config = module_args.is_empty();
        if !pkg.use_module && !pkg.use_config {
            return err(format!(
                "given options exclusive to Module mode:\n  {}\nand options exclusive to Config mode:\n  {}\nThe options are incompatible.",
                module_args.join("\n  "),
                config_args.join("\n  ")
            ));
        }
        if pkg.version.complete.is_empty() && pkg.version.exact {
            pkg.version.exact = false;
            interp.issue(
                MessageType::AuthorWarning,
                "Ignoring EXACT since no version is requested.",
            );
        }
        if interp.is_on(&format!("{name}_FIND_MODULE")) {
            if pkg.version.complete.is_empty() {
                pkg.version.complete = interp.get_safe(&format!("{name}_FIND_VERSION_COMPLETE"));
                pkg.version.exact = interp.is_on(&format!("{name}_FIND_VERSION_EXACT"));
            }
            if pkg.components.is_empty() {
                pkg.components = list(interp, &format!("{name}_FIND_COMPONENTS"));
                for component in &pkg.components {
                    if interp.is_on(&format!("{name}_FIND_REQUIRED_{component}")) {
                        pkg.required.insert(component.clone());
                    } else {
                        pkg.optional.insert(component.clone());
                    }
                }
            }
        }
        pkg.version.parse()?;
        Ok(pkg)
    }

    fn run(&mut self, interp: &mut Interp, args: &[String]) -> CmdResult {
        let name = &self.name;
        let force = format!("CMAKE_REQUIRE_FIND_PACKAGE_{name}");
        let forced = interp.is_on(&force);
        if forced {
            if self.requirement.required() {
                interp.issue(MessageType::Warning, &format!("for module {name} already called with REQUIRED, thus {force} has no effect."));
            } else {
                self.requirement = Requirement::PackageVariable;
            }
        }
        let disable = format!("CMAKE_DISABLE_FIND_PACKAGE_{name}");
        if interp.is_on(&disable) {
            if self.requirement.required() {
                let why = if forced {
                    format!("was made REQUIRED with {force}")
                } else {
                    "called with REQUIRED, ".into()
                };
                return err(format!(
                    "for module {name} {why} but {disable} is enabled. A REQUIRED package cannot be disabled."
                ));
            }
            return Ok(());
        }

        let original_prefix = interp.get_def("PACKAGE_PREFIX_DIR").map(str::to_owned);
        let result = self.with_provider(interp, args);
        restore(interp, "PACKAGE_PREFIX_DIR", original_prefix);
        result
    }

    fn with_provider(&mut self, interp: &mut Interp, args: &[String]) -> CmdResult {
        if let Some(command) = dependency_provider(interp, "FIND_PACKAGE").map(str::to_owned) {
            if self.bypass {
                self.debug_message(
                    interp,
                    "BYPASS_PROVIDER given, skipping dependency provider",
                );
            } else {
                self.debug_message(
                    interp,
                    &format!("Trying dependency provider command: {command}()"),
                );
                let mut provider_args = vec!["FIND_PACKAGE".into()];
                provider_args.extend_from_slice(args);
                if !interp.call_command(&command, &provider_args) {
                    return Err(CmdErr::Reported);
                }
                if interp.is_on(&format!("{}_FOUND", self.name)) {
                    self.debug_message(interp, "Package was found by the dependency provider");
                    self.record_result(interp);
                    return Ok(());
                }
            }
        }
        let limit = (interp
            .get_safe("CMAKE_MAXIMUM_RECURSION_DEPTH")
            .parse::<usize>()
            .unwrap_or(1000)
            / 2)
        .max(100);
        let directory = interp.mf_index();
        let state = interp.ext.find_package.entry(directory).or_default();
        if state.calls.len() >= limit {
            return err(format!("maximum nesting depth of {limit} exceeded."));
        }
        let roots = package_roots(interp, &self.name);
        interp
            .ext
            .find_package
            .entry(directory)
            .or_default()
            .push(self.name.clone(), roots);
        interp.ext.find_package_stack.push(self.name.clone());
        let saved: Vec<_> = self
            .request_definitions()
            .into_iter()
            .map(|(key, value)| {
                let old = interp.get_def(&key).map(str::to_owned);
                interp.set_def(&key, value);
                (key, old)
            })
            .collect();
        let result = self.load(interp);
        for (key, value) in saved {
            restore(interp, &key, value);
        }
        interp.ext.find_package.entry(directory).or_default().pop();
        interp.ext.find_package_stack.pop();
        if let Some(debug) = &self.debug
            && !debug.is_empty()
        {
            interp.issue(MessageType::Log, debug);
        }
        result
    }

    fn request_definitions(&self) -> Vec<(String, String)> {
        let name = &self.name;
        let prefix = format!("{name}_FIND_VERSION");
        let mut defs = vec![
            ("CMAKE_FIND_PACKAGE_NAME".into(), name.clone()),
            ("CMAKE_FIND_REQUIRED".into(), String::new()),
            (format!("{name}_FIND_COMPONENTS"), self.components.join(";")),
        ];
        for (set, required) in [(&self.optional, "0"), (&self.required, "1")] {
            defs.extend(
                set.iter().map(|component| {
                    (format!("{name}_FIND_REQUIRED_{component}"), required.into())
                }),
            );
        }
        if self.quiet {
            defs.push((format!("{name}_FIND_QUIETLY"), "1".into()));
        }
        if self.requirement.required() {
            defs.push((format!("{name}_FIND_REQUIRED"), "1".into()));
        }
        if !self.version.min.is_empty() {
            defs.push((format!("{prefix}_COMPLETE"), self.version.complete.clone()));
            defs.push((
                format!("{prefix}_EXACT"),
                u8::from(self.version.exact).to_string(),
            ));
            defs.extend(self.version.definitions(&prefix));
        }
        if let Some(view) = &self.registry_view {
            defs.push((format!("{name}_FIND_REGISTRY_VIEW"), view.clone()));
        }
        defs
    }

    fn load(&mut self, interp: &mut Interp) -> CmdResult {
        let redirect_names = if self.names.is_empty() {
            vec![self.name.clone()]
        } else {
            self.names.clone()
        };
        let redirects = interp.get_safe("CMAKE_FIND_PACKAGE_REDIRECTS_DIR");
        let mut redirected = false;
        for name in redirect_names {
            let lower = name.to_ascii_lowercase();
            let delegate = format!("_FetchContent_{lower}_override_find_package");
            if interp
                .global_props
                .get(&delegate)
                .is_some_and(crate::util::is_on)
                && !interp.call_command("FetchContent_MakeAvailable", std::slice::from_ref(&name))
            {
                return Err(CmdErr::Reported);
            }
            if [
                format!("{lower}-config.cmake"),
                format!("{name}Config.cmake"),
            ]
            .iter()
            .any(|file| access(&format!("{redirects}/{file}"), libc::R_OK))
            {
                redirected = true;
                self.use_module = false;
                self.use_config = true;
                self.names = vec![name];
                self.store_dir(interp, &redirects);
                break;
            }
        }

        let result = if redirected {
            self.config_mode(interp, false).map(|_| ())
        } else if interp.is_on("CMAKE_FIND_PACKAGE_PREFER_CONFIG") {
            match if self.use_config {
                self.config_mode(interp, self.use_module)
            } else {
                Ok(false)
            } {
                Ok(true) => Ok(()),
                Ok(false) => match self.module_mode(interp) {
                    Ok(false) => self.config_mode(interp, false).map(|_| ()),
                    result => result.map(|_| ()),
                },
                Err(error) => Err(error),
            }
        } else {
            match if self.use_module {
                self.module_mode(interp)
            } else {
                Ok(false)
            } {
                Ok(false) => {
                    if self.use_module
                        && self.use_config
                        && interp.is_on("CMAKE_FIND_PACKAGE_WARN_NO_MODULE")
                    {
                        self.warn_no_module(interp);
                    }
                    self.config_mode(interp, false).map(|_| ())
                }
                result => result.map(|_| ()),
            }
        };
        self.record_result(interp);
        result
    }

    fn module_mode(&mut self, interp: &mut Interp) -> Result<bool, CmdErr> {
        let filename = format!("Find{}.cmake", self.name);
        let system = format!("{}/Modules/{filename}", interp.cmake_root);
        let mut debug = format!("find_package considered the following paths for {filename}:\n");
        let local = list(interp, "CMAKE_MODULE_PATH")
            .into_iter()
            .find_map(|path| {
                let path = format!("{}/{filename}", convert_to_unix_slashes(&path));
                if access(&path, libc::R_OK) {
                    Some(path)
                } else {
                    let _ = writeln!(debug, "  {path}");
                    None
                }
            });
        let has_system = access(&system, libc::R_OK);
        if !has_system {
            let _ = writeln!(debug, "  {system}");
        }
        let current = interp.get_safe("CMAKE_CURRENT_LIST_FILE");
        let use_system = has_system
            && (local.is_none() || current.starts_with(&format!("{}/Modules/", interp.cmake_root)));
        let module = if use_system { Some(system) } else { local };
        let Some(module) = module else {
            debug.push_str("The file was not found.\n");
            self.append_debug(&debug);
            return Ok(false);
        };
        let _ = writeln!(debug, "The file was found at\n  {module}");
        self.append_debug(&debug);
        if use_system && let Some(policy) = deprecated_policy(&self.name) {
            match interp.policy_status(&format!("CMP{policy:04}")) {
                PolicyStatus::New => return Ok(false),
                PolicyStatus::Warn => interp.issue(
                    MessageType::AuthorWarning,
                    &format!("{}\n", policy_warning(policy)),
                ),
                PolicyStatus::Old => {}
            }
        }
        let marker = format!("{}_FIND_MODULE", self.name);
        interp.set_def(&marker, "1");
        let result = self.read(interp, &module, false);
        interp.unset_def(&marker);
        let found = format!("{}_FOUND", self.name);
        if interp.is_def_set(&found) && !interp.is_on(&found) {
            self.append_debug(&format!(
                "The module is considered not found due to {found} being FALSE."
            ));
        }
        result.map(|_| true)
    }

    fn config_mode(&mut self, interp: &mut Interp, fall_back: bool) -> Result<bool, CmdErr> {
        self.considered.clear();
        if self.use_config {
            if self.names.is_empty() {
                self.names.push(self.name.clone());
            }
            if self.configs.is_empty() {
                for name in &self.names {
                    self.configs.push(format!("{name}Config.cmake"));
                    self.configs
                        .push(format!("{}-config.cmake", name.to_ascii_lowercase()));
                }
            }
        }

        let mut file = None;
        if self.use_config {
            let dir = interp.get_safe(&self.dir);
            if !is_off_opt(Some(&dir)) {
                let dir = convert_to_unix_slashes(&dir);
                let dir = if is_full_path(&dir) {
                    dir
                } else {
                    format!("{}/{dir}", interp.current_source_dir())
                };
                file = self.find_config_file(interp, &dir);
            }
            if file.is_none() {
                file = self.search_config(interp);
                let dir = file.as_ref().map_or_else(
                    || format!("{}-NOTFOUND", self.dir),
                    |file| filename_path(file),
                );
                self.store_dir(interp, &dir);
            }
        }

        let name = self.name.clone();
        let found_var = format!("{name}_FOUND");
        let message_var = format!("{name}_NOT_FOUND_MESSAGE");
        let mut rejected = None;
        let mut found = false;
        let result = if let Some(file) = &file {
            if interp.is_def_set(&found_var) && !interp.is_on(&found_var) {
                interp.unset_def(&found_var);
            }
            interp.unset_def(&message_var);
            let version_var = format!("{name}_VERSION");
            for (key, value) in version_vars(&version_var, &self.selected_version) {
                interp.set_def(key, value);
            }
            if self.selected_version.is_empty() {
                interp.unset_def(&version_var);
            }
            let result = self.read(interp, file, false);
            if result.is_ok() {
                found = !interp.is_def_set(&found_var) || interp.is_on(&found_var);
                if !found {
                    rejected = Some(interp.get_safe(&message_var));
                }
            }
            result
        } else {
            Ok(())
        };

        if result.is_ok() && !found && !fall_back {
            self.report_missing(interp, file.as_deref(), rejected.as_deref());
        }
        interp.set_def(&found_var, if found { "1" } else { "0" });
        restore(interp, &format!("{name}_CONFIG"), file.filter(|_| found));
        let paths = self
            .considered
            .iter()
            .map(|(path, _)| path.as_str())
            .collect::<Vec<_>>()
            .join(";");
        let versions = self
            .considered
            .iter()
            .map(|(_, version)| version.as_str())
            .collect::<Vec<_>>()
            .join(";");
        interp.set_def(format!("{name}_CONSIDERED_CONFIGS"), paths);
        interp.set_def(format!("{name}_CONSIDERED_VERSIONS"), versions);
        result.map(|_| found || !fall_back)
    }

    fn read(&self, interp: &mut Interp, file: &str, version: bool) -> CmdResult {
        let directory = interp.mf_index();
        let state = interp.ext.find_package.entry(directory).or_default();
        let previous = state.global;
        state.global |= self.global;
        let read = interp.read_dependent_file(file, !version && self.no_policy_scope);
        interp.ext.find_package.entry(directory).or_default().global = previous;
        if read {
            Ok(())
        } else {
            err(format!("Error reading CMake code from \"{file}\"."))
        }
    }

    fn store_dir(&self, interp: &mut Interp, value: &str) {
        let doc = format!(
            "The directory containing a CMake configuration file for {}.",
            self.name
        );
        cache_result(interp, &self.dir, value, &doc, CacheType::Path, true);
        if interp.policy_status("CMP0126") == PolicyStatus::New
            && interp.get_normal_def(&self.dir).is_some()
        {
            interp.set_def(&self.dir, value);
        }
    }

    fn report_missing(&mut self, interp: &mut Interp, file: Option<&str>, rejection: Option<&str>) {
        let name = &self.name;
        let required = self.requirement.required();
        if !required
            && (self.quiet
                || self.requirement == Requirement::Optional
                || (self.use_config && !self.use_module && self.considered.is_empty()))
        {
            if !self.quiet {
                interp.stdout_line(&format!("-- Could NOT find {name} (missing: {name}_DIR)"));
            }
            return;
        }
        let mut author = None;
        let mut message = if let (Some(file), Some(rejection)) = (file, rejection) {
            let mut text = format!(
                "Found package configuration file:\n  {file}\nbut it set {name}_FOUND to FALSE so package \"{name}\" is considered to be NOT FOUND."
            );
            if !rejection.is_empty() {
                let _ = writeln!(text, " Reason given by package: \n{rejection}");
            }
            text
        } else if !self.considered.is_empty() {
            let comparison = if self.version.exact {
                "exactly matches"
            } else {
                "is compatible with"
            };
            let range = if self.version.max.is_empty() {
                ""
            } else {
                "range "
            };
            let mut text = format!(
                "Could not find a configuration file for package \"{name}\" that {comparison} requested version {range}\"{}\".\nThe following configuration files were considered but not accepted:\n",
                self.version.complete
            );
            // cmRemoveDuplicates compacts the entries used for this
            // diagnostic. The reported list variables keep the vector length.
            let mut seen = HashSet::new();
            let mut distinct = 0;
            for index in 0..self.considered.len() {
                if seen.insert(self.considered[index].0.clone()) {
                    if index != distinct {
                        self.considered[distinct] = std::mem::take(&mut self.considered[index]);
                    }
                    let (file, version) = &self.considered[distinct];
                    let _ = writeln!(text, "  {file}, version: {version}");
                    distinct += 1;
                }
            }
            text
        } else if self.use_config {
            let mut text = String::new();
            if self.use_module {
                let _ = writeln!(
                    text,
                    "By not providing \"Find{name}.cmake\" in CMAKE_MODULE_PATH this project has asked CMake to find a package configuration file provided by \"{name}\", but CMake did not find one."
                );
            }
            let requested = if self.version.complete.is_empty() {
                String::new()
            } else {
                format!(" (requested version {})", self.version.complete)
            };
            if let [config] = self.configs.as_slice() {
                let _ = writeln!(
                    text,
                    "Could not find a package configuration file named \"{config}\" provided by package \"{name}\"{requested}."
                );
            } else {
                let _ = writeln!(
                    text,
                    "Could not find a package configuration file provided by \"{name}\"{requested} with any of the following names:\n  {}",
                    self.configs.join("\n  ")
                );
            }
            let _ = write!(
                text,
                "Add the installation prefix of \"{name}\" to CMAKE_PREFIX_PATH or set \"{}\" to a directory containing one of the above files. If \"{name}\" provides a separate development package or SDK, be sure it has been installed.",
                self.dir
            );
            text
        } else {
            author = Some(format!(
                "Find{name}.cmake must either be part of this project itself, in this case adjust CMAKE_MODULE_PATH so that it points to the correct location inside its source tree.\nOr it must be installed by a package which has already been found via find_package().  In this case make sure that package has indeed been found and adjust CMAKE_MODULE_PATH to contain the location where that package has installed Find{name}.cmake.  This must be a location provided by that package.  This error in general means that the buildsystem of this project is relying on a Find-module without ensuring that it is actually available.\n"
            ));
            format!("No \"Find{name}.cmake\" found in CMAKE_MODULE_PATH.")
        };
        match self.requirement {
            Requirement::FindVariable => message.push_str("\nThis package is considered required because the CMAKE_FIND_REQUIRED variable has been enabled.\n"),
            Requirement::PackageVariable => { let _ = writeln!(message, "\nThis package is considered required because the CMAKE_REQUIRE_FIND_PACKAGE_{name} variable has been enabled."); }
            _ => {}
        }
        interp.issue(
            if required {
                MessageType::FatalError
            } else {
                MessageType::Warning
            },
            &message,
        );
        if required {
            interp.set_fatal();
        }
        if let Some(author) = author {
            interp.issue(MessageType::AuthorWarning, &author);
        }
    }

    fn warn_no_module(&self, interp: &mut Interp) {
        let name = &self.name;
        let minimum = interp.get_safe("CMAKE_MINIMUM_REQUIRED_VERSION");
        let lower = name.to_ascii_lowercase();
        let text = if version_compare(CompareOp::GreaterEqual, &minimum, "2.8.8") {
            format!(
                "find_package called without either MODULE or CONFIG option and no Find{name}.cmake module is in CMAKE_MODULE_PATH.  Add MODULE to exclusively request Module mode and fail if Find{name}.cmake is missing.  Add CONFIG to exclusively request Config mode and search for a package configuration file provided by {name} ({name}Config.cmake or {lower}-config.cmake).  "
            )
        } else {
            format!(
                "find_package called without NO_MODULE option and no Find{name}.cmake module is in CMAKE_MODULE_PATH.  Add NO_MODULE to exclusively request Config mode and search for a package configuration file provided by {name} ({name}Config.cmake or {lower}-config.cmake).  Otherwise make Find{name}.cmake available in CMAKE_MODULE_PATH."
            )
        };
        interp.issue(
            MessageType::AuthorWarning,
            &format!("{text}\n(Variable CMAKE_FIND_PACKAGE_WARN_NO_MODULE enabled this warning.)"),
        );
    }

    fn record_result(&self, interp: &mut Interp) {
        let name = &self.name;
        let found = format!("{name}_FOUND");
        let found = interp.is_on(&found) || interp.is_on(&found.to_ascii_uppercase());
        for (key, append) in [("PACKAGES_FOUND", found), ("PACKAGES_NOT_FOUND", !found)] {
            let mut items = expand_list(interp.global_props.get(key).unwrap_or(""), false);
            items.retain(|pkg| pkg != name);
            if append {
                items.push(name.clone());
            }
            interp.global_props.set(key, items.join(";"));
        }
        let prefix = format!("_CMAKE_{name}");
        interp
            .global_props
            .set(&format!("{prefix}_TRANSITIVE_DEPENDENCY"), "False".into());
        interp.global_props.set(
            &format!("{prefix}_QUIET"),
            if self.quiet { "TRUE" } else { "FALSE" }.into(),
        );
        let version = if self.version.min.is_empty() {
            String::new()
        } else if self.version.max.is_empty() {
            let op = if self.version.exact { "==" } else { ">=" };
            format!("{op} {}", self.version.min)
        } else {
            self.version.complete.clone()
        };
        interp
            .global_props
            .set(&format!("{prefix}_REQUIRED_VERSION"), version);
        if self.requirement.required() {
            interp
                .global_props
                .set(&format!("{prefix}_TYPE"), "REQUIRED".into());
        }
    }

    fn debug_message(&self, interp: &mut Interp, message: &str) {
        if self.debug.is_some() {
            interp.issue(MessageType::Log, message);
        }
    }

    fn append_debug(&mut self, message: &str) {
        if let Some(debug) = &mut self.debug {
            debug.push_str(message);
        }
    }
}

fn find_package(interp: &mut Interp, args: &[String]) -> CmdResult {
    Package::parse(interp, args)?.run(interp, args)
}

fn restore(interp: &mut Interp, key: &str, value: Option<String>) {
    if let Some(value) = value {
        interp.set_def(key, value);
    } else {
        interp.unset_def(key);
    }
}

fn version_vars(prefix: &str, version: &str) -> Vec<(String, String)> {
    let mut vars = vec![(prefix.to_owned(), version.to_owned())];
    let [mut major, mut minor, mut patch, mut tweak] = [0u32; 4];
    let count = CString::new(version).map_or(0, |version| {
        // The format specifies four unsigned integers, each backed by a
        // distinct writable u32. Missing components retain their zero values.
        unsafe {
            libc::sscanf(
                version.as_ptr(),
                c"%u.%u.%u.%u".as_ptr(),
                &mut major,
                &mut minor,
                &mut patch,
                &mut tweak,
            )
        }
        .max(0)
    });
    for (label, value) in [
        ("MAJOR", major),
        ("MINOR", minor),
        ("PATCH", patch),
        ("TWEAK", tweak),
    ] {
        vars.push((format!("{prefix}_{label}"), value.to_string()));
    }
    vars.push((format!("{prefix}_COUNT"), count.to_string()));
    vars
}

fn deprecated_policy(name: &str) -> Option<usize> {
    match name {
        "Boost" => Some(167),
        "CABLE" => Some(191),
        "CUDA" => Some(146),
        "Dart" => Some(145),
        "GCCXML" => Some(188),
        "PythonInterp" | "PythonLibs" => Some(148),
        "Qt" => Some(84),
        _ => None,
    }
}

fn registry_disabled(interp: &Interp, system: &str) -> bool {
    let use_var = format!("CMAKE_FIND_USE_{system}PACKAGE_REGISTRY");
    interp.get_def(&use_var).map_or_else(
        || interp.is_on(&format!("CMAKE_FIND_PACKAGE_NO_{system}PACKAGE_REGISTRY")),
        |value| !crate::util::is_on(value),
    )
}

fn package_roots(interp: &mut Interp, name: &str) -> Vec<String> {
    let key = format!("{name}_ROOT");
    let upper_key = key.to_ascii_uppercase();
    let normal = interp
        .get_def(&key)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let environment = env::var(&key).ok().filter(|value| !value.is_empty());
    match interp.policy_status("CMP0074") {
        PolicyStatus::Old => return Vec::new(),
        PolicyStatus::Warn => {
            warn_root(interp, 74, &key, normal.as_deref(), environment.as_deref());
            return Vec::new();
        }
        PolicyStatus::New => {}
    }
    let (mut upper, mut upper_env) = (None, None);
    if key != upper_key {
        upper = interp
            .get_def(&upper_key)
            .filter(|value| !value.is_empty() && normal.as_deref() != Some(value))
            .map(str::to_owned);
        upper_env = env::var(&upper_key)
            .ok()
            .filter(|value| !value.is_empty() && Some(value) != environment.as_ref());
        if interp.policy_status("CMP0144") != PolicyStatus::New {
            if interp.policy_status("CMP0144") == PolicyStatus::Warn {
                warn_root(
                    interp,
                    144,
                    &upper_key,
                    upper.as_deref(),
                    upper_env.as_deref(),
                );
            }
            upper = None;
            upper_env = None;
        }
    }
    let mut result = Vec::new();
    for value in [normal, upper].into_iter().flatten() {
        result.extend(expand_list(&value, false));
    }
    for value in [environment, upper_env].into_iter().flatten() {
        result.extend(split_env_paths(&value));
    }
    result
}

fn warn_root(
    interp: &mut Interp,
    policy: usize,
    key: &str,
    value: Option<&str>,
    env: Option<&str>,
) {
    if value.is_none() && env.is_none() {
        return;
    }
    let directory = interp.mf_index();
    let state = interp.ext.find_package.entry(directory).or_default();
    if !state.warned_roots.insert((policy, key.to_owned())) {
        return;
    }
    let mut text = format!("{}\n", policy_warning(policy));
    for (kind, value) in [("CMake", value), ("Environment", env)] {
        if let Some(value) = value {
            let _ = writeln!(text, "{kind} variable {key} is set to:\n  {value}");
        }
    }
    text.push_str(if policy == 74 { "For compatibility, CMake is ignoring the variable." }
        else { "For compatibility, find_package is ignoring the variable, but code in a .cmake module might still use it." });
    interp.issue(MessageType::AuthorWarning, &text);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interp::Mode;

    #[test]
    fn provider_receives_expanded_arguments_and_can_delegate() {
        let mut interp = Interp::new(Mode::Script, "/", "/", "/");
        interp.ext.dependency_provider = Some(("provide".into(), vec!["FIND_PACKAGE".into()]));
        let script = r#"
            macro(provide method name version)
              set(provider_invocation "${method}|${name}|${version}")
              set(PACKAGE_PREFIX_DIR inner)
              if("${name}" STREQUAL "Delegated")
                find_package(${name} QUIET CONFIG NO_DEFAULT_PATH BYPASS_PROVIDER)
              else()
                set(${name}_FOUND TRUE)
              endif()
            endmacro()
            set(PACKAGE_PREFIX_DIR outer)
            find_package(Provided 4.1 REQUIRED)
            set(first_invocation "${provider_invocation}")
            find_package(Delegated 3)
        "#;
        assert!(interp.run_string(script, "/test.cmake"));
        assert!(!interp.error);
        assert_eq!(
            interp.get_def("first_invocation"),
            Some("FIND_PACKAGE|Provided|4.1")
        );
        assert_eq!(
            interp.get_def("provider_invocation"),
            Some("FIND_PACKAGE|Delegated|3")
        );
        assert_eq!(interp.get_def("Provided_FOUND"), Some("TRUE"));
        assert_eq!(interp.get_def("Delegated_FOUND"), Some("0"));
        assert_eq!(interp.get_def("PACKAGE_PREFIX_DIR"), Some("outer"));
    }
}
