//! CMake presets: `cmCMakePresetsGraph` (cmCMakePresetsGraph.cxx,
//! cmCMakePresetsGraphInternal.h, cmCMakePresetsGraphReadJSON*.cxx).

pub mod cli;
mod errors;
pub mod json;
mod read;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use json::State;

// ---------------------------------------------------------------------
// Preset data model (cmCMakePresetsGraph.h)

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArchToolsetStrategy {
    Set,
    External,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TraceEnableMode {
    Disable,
    Default,
    Expand,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TraceOutputFormat {
    Human,
    JsonV1,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PackageResolveMode {
    Force,
    Disable,
    OnlyResolve,
}

#[derive(Clone, Default, Debug)]
pub struct CacheVariable {
    pub type_: String,
    pub value: String,
}

/// `cmCMakePresetsGraph::Condition` hierarchy.
#[derive(Clone, Debug)]
pub enum Condition {
    Null,
    Const(bool),
    Equals {
        lhs: String,
        rhs: String,
    },
    InList {
        string: String,
        list: Vec<String>,
    },
    Matches {
        string: String,
        regex: String,
    },
    AnyAllOf {
        conditions: Vec<Condition>,
        stop_value: bool,
    },
    Not(Box<Condition>),
}

impl Condition {
    pub fn is_null(&self) -> bool {
        matches!(self, Condition::Null)
    }

    /// `Condition::Evaluate`: `Err` for a false return, `Ok(None)` for an
    /// ignored macro.
    fn evaluate(&self, expanders: &[Expander], version: i32) -> Result<Option<bool>, ()> {
        macro_rules! check_expand {
            ($field:expr) => {
                match expand_macros(&mut $field, expanders, version) {
                    ExpandResult::Error => return Err(()),
                    ExpandResult::Ignore => return Ok(None),
                    ExpandResult::Ok => {}
                }
            };
        }
        match self {
            Condition::Null => Ok(Some(true)),
            Condition::Const(value) => Ok(Some(*value)),
            Condition::Equals { lhs, rhs } => {
                let mut lhs = lhs.clone();
                check_expand!(lhs);
                let mut rhs = rhs.clone();
                check_expand!(rhs);
                Ok(Some(lhs == rhs))
            }
            Condition::InList { string, list } => {
                let mut string = string.clone();
                check_expand!(string);
                for item in list {
                    let mut item = item.clone();
                    check_expand!(item);
                    if string == item {
                        return Ok(Some(true));
                    }
                }
                Ok(Some(false))
            }
            Condition::Matches { string, regex } => {
                let mut string = string.clone();
                check_expand!(string);
                let mut regex = regex.clone();
                check_expand!(regex);
                let Ok(regex) = crate::cmregex::Regex::new(&regex) else {
                    return Err(());
                };
                Ok(Some(regex.find(&string).is_some()))
            }
            Condition::AnyAllOf {
                conditions,
                stop_value,
            } => {
                for condition in conditions {
                    let Some(result) = condition.evaluate(expanders, version)? else {
                        return Ok(None);
                    };
                    if result == *stop_value {
                        return Ok(Some(result));
                    }
                }
                Ok(Some(!*stop_value))
            }
            Condition::Not(sub) => Ok(sub.evaluate(expanders, version)?.map(|v| !v)),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Base {
    pub name: String,
    pub inherits: Vec<String>,
    pub hidden: bool,
    /// Index into `Graph::files`.
    pub origin_file: usize,
    pub display_name: String,
    pub description: String,
    pub condition: Option<Rc<Condition>>,
    pub condition_result: bool,
    pub environment: BTreeMap<String, Option<String>>,
}

impl Default for Base {
    fn default() -> Self {
        Base {
            name: String::new(),
            inherits: Vec::new(),
            hidden: false,
            origin_file: 0,
            display_name: String::new(),
            description: String::new(),
            condition: None,
            condition_result: true,
            environment: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Default, Debug)]
pub struct ConfigurePreset {
    pub base: Base,
    pub generator: String,
    pub architecture: String,
    pub architecture_strategy: Option<ArchToolsetStrategy>,
    pub toolset: String,
    pub toolset_strategy: Option<ArchToolsetStrategy>,
    pub toolchain_file: String,
    pub graphviz_file: String,
    pub binary_dir: String,
    pub install_dir: String,
    pub cache_variables: BTreeMap<String, Option<CacheVariable>>,
    pub warn_dev: Option<bool>,
    pub error_dev: Option<bool>,
    pub warn_deprecated: Option<bool>,
    pub error_deprecated: Option<bool>,
    pub warn_uninitialized: Option<bool>,
    pub warn_unused_cli: Option<bool>,
    pub warn_system_vars: Option<bool>,
    pub debug_output: Option<bool>,
    pub debug_try_compile: Option<bool>,
    pub debug_find: Option<bool>,
    pub trace_mode: Option<TraceEnableMode>,
    pub trace_format: Option<TraceOutputFormat>,
    pub trace_source: Vec<String>,
    pub trace_redirect: String,
}

#[derive(Clone, Default, Debug)]
pub struct BuildPreset {
    pub base: Base,
    pub configure_preset: String,
    pub inherit_configure_environment: Option<bool>,
    pub jobs: Option<i32>,
    pub targets: Vec<String>,
    pub configuration: String,
    pub clean_first: Option<bool>,
    pub verbose: Option<bool>,
    pub native_tool_options: Vec<String>,
    pub resolve_package_references: Option<PackageResolveMode>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Verbosity {
    #[default]
    Default,
    Verbose,
    Extra,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TruncationMode {
    Tail,
    Middle,
    Head,
}

#[derive(Clone, Default, Debug)]
pub struct OutputOptions {
    pub short_progress: Option<bool>,
    pub verbosity: Option<Verbosity>,
    pub debug: Option<bool>,
    pub output_on_failure: Option<bool>,
    pub quiet: Option<bool>,
    pub output_log_file: String,
    pub output_junit_file: String,
    pub label_summary: Option<bool>,
    pub subproject_summary: Option<bool>,
    pub max_passed_test_output_size: Option<i32>,
    pub max_failed_test_output_size: Option<i32>,
    pub test_output_truncation: Option<TruncationMode>,
    pub max_test_name_width: Option<i32>,
}

#[derive(Clone, Default, Debug)]
pub struct IndexOptions {
    pub start: Option<i32>,
    pub end: Option<i32>,
    pub stride: Option<i32>,
    pub specific_tests: Vec<i32>,
    pub index_file: String,
}

#[derive(Clone, Default, Debug)]
pub struct IncludeOptions {
    pub name: String,
    pub label: String,
    pub index: Option<IndexOptions>,
    pub use_union: Option<bool>,
}

#[derive(Clone, Default, Debug)]
pub struct FixturesOptions {
    pub any: String,
    pub setup: String,
    pub cleanup: String,
}

#[derive(Clone, Default, Debug)]
pub struct ExcludeOptions {
    pub name: String,
    pub label: String,
    pub fixtures: Option<FixturesOptions>,
}

#[derive(Clone, Default, Debug)]
pub struct FilterOptions {
    pub include: Option<IncludeOptions>,
    pub exclude: Option<ExcludeOptions>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ShowOnly {
    #[default]
    Human,
    JsonV1,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum RepeatMode {
    #[default]
    UntilFail,
    UntilPass,
    AfterTimeout,
}

#[derive(Clone, Default, Debug)]
pub struct RepeatOptions {
    pub mode: RepeatMode,
    pub count: i32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum NoTestsAction {
    #[default]
    Default,
    Error,
    Ignore,
}

#[derive(Clone, Default, Debug)]
pub struct ExecutionOptions {
    pub stop_on_failure: Option<bool>,
    pub enable_failover: Option<bool>,
    pub jobs: Option<i32>,
    pub resource_spec_file: String,
    pub test_load: Option<i32>,
    pub show_only: Option<ShowOnly>,
    pub repeat: Option<RepeatOptions>,
    pub interactive_debugging: Option<bool>,
    pub schedule_random: Option<bool>,
    pub timeout: Option<i32>,
    pub no_tests_action: Option<NoTestsAction>,
}

#[derive(Clone, Default, Debug)]
pub struct TestPreset {
    pub base: Base,
    pub configure_preset: String,
    pub inherit_configure_environment: Option<bool>,
    pub configuration: String,
    pub overwrite_configuration_file: Vec<String>,
    pub output: Option<OutputOptions>,
    pub filter: Option<FilterOptions>,
    pub execution: Option<ExecutionOptions>,
}

#[derive(Clone, Default, Debug)]
pub struct PackagePreset {
    pub base: Base,
    pub configure_preset: String,
    pub inherit_configure_environment: Option<bool>,
    pub generators: Vec<String>,
    pub configurations: Vec<String>,
    pub variables: BTreeMap<String, String>,
    pub config_file: String,
    pub debug_output: Option<bool>,
    pub verbose_output: Option<bool>,
    pub package_name: String,
    pub package_version: String,
    pub package_directory: String,
    pub vendor_name: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum StepType {
    #[default]
    Configure,
    Build,
    Test,
    Package,
}

#[derive(Clone, Default, Debug)]
pub struct WorkflowStep {
    pub preset_type: StepType,
    pub preset_name: String,
}

#[derive(Clone, Default, Debug)]
pub struct WorkflowPreset {
    pub base: Base,
    pub steps: Vec<WorkflowStep>,
}

#[derive(Clone, Default, Debug)]
pub struct PresetPair<T> {
    pub unexpanded: T,
    pub expanded: Option<T>,
}

pub struct File {
    pub filename: String,
    pub version: i32,
    pub reachable: BTreeSet<usize>,
}

// ---------------------------------------------------------------------
// Macro expansion (cmCMakePresetsGraphInternal)

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExpandResult {
    Ok,
    Ignore,
    Error,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CycleStatus {
    Unvisited,
    InProgress,
    Verified,
}

#[derive(Default)]
pub(crate) struct EnvContext {
    env: BTreeMap<String, Option<String>>,
    cycles: BTreeMap<String, CycleStatus>,
}

/// The `MacroExpander` implementations.
pub enum Expander<'a> {
    /// `BaseMacroExpander`.
    Base {
        source_dir: &'a str,
        file: Option<String>,
    },
    /// `PresetMacroExpander<T>`.
    Preset {
        name: String,
        hidden: bool,
        generator: String,
        origin_file: String,
    },
    /// `EnvironmentMacroExpander<T>` from cmCMakePresetsGraph.cxx.
    Environment(&'a RefCell<EnvContext>),
    /// The `penv`-only `EnvironmentMacroExpander` used for includes.
    ProcessEnvironment,
}

fn get_env(name: &str) -> Option<String> {
    std::env::var_os(name).map(|v| v.to_string_lossy().into_owned())
}

impl Expander<'_> {
    fn expand(
        &self,
        ns: &str,
        name: &str,
        out: &mut String,
        version: i32,
        all: &[Expander],
    ) -> ExpandResult {
        match self {
            Expander::Base { source_dir, file } => {
                if ns.is_empty() {
                    match name {
                        "sourceDir" => {
                            out.push_str(source_dir);
                            return ExpandResult::Ok;
                        }
                        "sourceParentDir" => {
                            out.push_str(&crate::util::filename_path(source_dir));
                            return ExpandResult::Ok;
                        }
                        "sourceDirName" => {
                            out.push_str(&crate::util::filename_name(source_dir));
                            return ExpandResult::Ok;
                        }
                        "dollar" => {
                            out.push('$');
                            return ExpandResult::Ok;
                        }
                        "hostSystemName" => {
                            if version < 3 {
                                return ExpandResult::Error;
                            }
                            // cmSystemTools::GetSystemName
                            out.push_str(if cfg!(target_os = "macos") {
                                "Darwin"
                            } else {
                                "Linux"
                            });
                            return ExpandResult::Ok;
                        }
                        "fileDir" if file.is_some() => {
                            if version < 4 {
                                return ExpandResult::Error;
                            }
                            out.push_str(&crate::util::filename_path(
                                file.as_deref().unwrap_or_default(),
                            ));
                            return ExpandResult::Ok;
                        }
                        "pathListSep" => {
                            if version < 5 {
                                return ExpandResult::Error;
                            }
                            out.push(':');
                            return ExpandResult::Ok;
                        }
                        _ => {}
                    }
                }
                ExpandResult::Ignore
            }
            Expander::Preset {
                name: preset_name,
                hidden,
                generator,
                origin_file,
            } => {
                if ns.is_empty() {
                    match name {
                        "presetName" => {
                            out.push_str(preset_name);
                            return ExpandResult::Ok;
                        }
                        "generator" => {
                            if !hidden {
                                out.push_str(generator);
                            }
                            return ExpandResult::Ok;
                        }
                        "fileDir" => {
                            if version < 4 {
                                return ExpandResult::Error;
                            }
                            out.push_str(&crate::util::filename_path(origin_file));
                            return ExpandResult::Ok;
                        }
                        _ => {}
                    }
                }
                ExpandResult::Ignore
            }
            Expander::Environment(ctx) => {
                if ns == "env" && !name.is_empty() {
                    let entry = ctx.borrow().env.get(name).cloned();
                    if let Some(Some(mut value)) = entry {
                        let e = visit_env(&mut value, name, ctx, all, version);
                        if e != ExpandResult::Ok {
                            return e;
                        }
                        ctx.borrow_mut()
                            .env
                            .insert(name.to_owned(), Some(value.clone()));
                        out.push_str(&value);
                        return ExpandResult::Ok;
                    }
                }
                if ns == "env" || ns == "penv" {
                    if name.is_empty() {
                        return ExpandResult::Error;
                    }
                    if let Some(value) = get_env(name) {
                        out.push_str(&value);
                    }
                    return ExpandResult::Ok;
                }
                ExpandResult::Ignore
            }
            Expander::ProcessEnvironment => {
                if ns == "penv" {
                    if name.is_empty() {
                        return ExpandResult::Error;
                    }
                    if let Some(value) = get_env(name) {
                        out.push_str(&value);
                    }
                    return ExpandResult::Ok;
                }
                ExpandResult::Ignore
            }
        }
    }
}

/// `VisitEnv`.
fn visit_env(
    value: &mut String,
    key: &str,
    ctx: &RefCell<EnvContext>,
    all: &[Expander],
    version: i32,
) -> ExpandResult {
    let status = *ctx
        .borrow_mut()
        .cycles
        .entry(key.to_owned())
        .or_insert(CycleStatus::Unvisited);
    match status {
        CycleStatus::Verified => return ExpandResult::Ok,
        CycleStatus::InProgress => return ExpandResult::Error,
        CycleStatus::Unvisited => {}
    }
    ctx.borrow_mut()
        .cycles
        .insert(key.to_owned(), CycleStatus::InProgress);
    let e = expand_macros(value, all, version);
    if e != ExpandResult::Ok {
        return e;
    }
    ctx.borrow_mut()
        .cycles
        .insert(key.to_owned(), CycleStatus::Verified);
    ExpandResult::Ok
}

const VALID_PREFIXES: [&str; 4] = ["", "env", "penv", "vendor"];

/// `cmCMakePresetsGraphInternal::ExpandMacros`.
fn expand_macros(out: &mut String, expanders: &[Expander], version: i32) -> ExpandResult {
    #[derive(PartialEq)]
    enum St {
        Default,
        Namespace,
        Name,
    }
    let mut result: Vec<u8> = Vec::new();
    let mut ns: Vec<u8> = Vec::new();
    let mut name: Vec<u8> = Vec::new();
    let mut state = St::Default;
    for &c in out.as_bytes() {
        match state {
            St::Default => {
                if c == b'$' {
                    state = St::Namespace;
                } else {
                    result.push(c);
                }
            }
            St::Namespace => {
                if c == b'{' {
                    if VALID_PREFIXES.iter().any(|p| p.as_bytes() == ns.as_slice()) {
                        state = St::Name;
                    } else {
                        result.push(b'$');
                        result.extend_from_slice(&ns);
                        result.push(b'{');
                        ns.clear();
                        state = St::Default;
                    }
                } else {
                    ns.push(c);
                    if !VALID_PREFIXES.iter().any(|p| p.as_bytes().starts_with(&ns)) {
                        result.push(b'$');
                        result.extend_from_slice(&ns);
                        ns.clear();
                        state = St::Default;
                    }
                }
            }
            St::Name => {
                if c == b'}' {
                    let mut text = String::from_utf8_lossy(&result).into_owned();
                    let e = expand_macro(
                        &mut text,
                        &String::from_utf8_lossy(&ns),
                        &String::from_utf8_lossy(&name),
                        expanders,
                        version,
                    );
                    if e != ExpandResult::Ok {
                        return e;
                    }
                    result = text.into_bytes();
                    ns.clear();
                    name.clear();
                    state = St::Default;
                } else {
                    name.push(c);
                }
            }
        }
    }
    match state {
        St::Default => {}
        St::Namespace => {
            result.push(b'$');
            result.extend_from_slice(&ns);
        }
        St::Name => return ExpandResult::Error,
    }
    *out = String::from_utf8_lossy(&result).into_owned();
    ExpandResult::Ok
}

/// `cmCMakePresetsGraphInternal::ExpandMacro`.
fn expand_macro(
    out: &mut String,
    ns: &str,
    name: &str,
    expanders: &[Expander],
    version: i32,
) -> ExpandResult {
    for expander in expanders {
        let result = expander.expand(ns, name, out, version, expanders);
        if result != ExpandResult::Ignore {
            return result;
        }
    }
    if ns == "vendor" {
        return ExpandResult::Ignore;
    }
    ExpandResult::Error
}

// ---------------------------------------------------------------------
// Per-kind behavior (VisitPreset* and ExpandMacros overloads)

pub trait PresetKind: Clone + Default + 'static {
    fn base(&self) -> &Base;
    fn base_mut(&mut self) -> &mut Base;
    fn visit_inherit(&mut self, parent: &Self) -> bool;
    fn visit_before_inherit(&self) -> bool {
        true
    }
    fn visit_after_inherit(&self, _version: i32, _state: &mut State) -> bool {
        true
    }
    /// The type-specific `ExpandMacros` overload.
    fn expand_fields(
        graph: &Graph,
        preset: &Self,
        out: &mut Self,
        expanders: &[Expander],
    ) -> ExpandResult;
}

fn inherit_string(child: &mut String, parent: &str) {
    if child.is_empty() {
        *child = parent.to_owned();
    }
}

fn inherit_optional<T: Clone>(child: &mut Option<T>, parent: &Option<T>) {
    if child.is_none() {
        *child = parent.clone();
    }
}

fn inherit_vector<T: Clone>(child: &mut Vec<T>, parent: &[T]) {
    if child.is_empty() {
        *child = parent.to_vec();
    }
}

macro_rules! check_expand {
    ($field:expr, $expanders:expr, $version:expr) => {
        match expand_macros(&mut $field, $expanders, $version) {
            ExpandResult::Ok => {}
            other => return other,
        }
    };
}

impl PresetKind for ConfigurePreset {
    fn base(&self) -> &Base {
        &self.base
    }
    fn base_mut(&mut self) -> &mut Base {
        &mut self.base
    }

    fn visit_inherit(&mut self, parent: &Self) -> bool {
        inherit_string(&mut self.generator, &parent.generator);
        inherit_string(&mut self.architecture, &parent.architecture);
        inherit_string(&mut self.toolset, &parent.toolset);
        inherit_optional(
            &mut self.architecture_strategy,
            &parent.architecture_strategy,
        );
        inherit_optional(&mut self.toolset_strategy, &parent.toolset_strategy);
        inherit_string(&mut self.binary_dir, &parent.binary_dir);
        inherit_string(&mut self.install_dir, &parent.install_dir);
        inherit_string(&mut self.toolchain_file, &parent.toolchain_file);
        inherit_string(&mut self.graphviz_file, &parent.graphviz_file);
        inherit_optional(&mut self.warn_dev, &parent.warn_dev);
        inherit_optional(&mut self.error_dev, &parent.error_dev);
        inherit_optional(&mut self.warn_deprecated, &parent.warn_deprecated);
        inherit_optional(&mut self.error_deprecated, &parent.error_deprecated);
        inherit_optional(&mut self.warn_uninitialized, &parent.warn_uninitialized);
        inherit_optional(&mut self.warn_unused_cli, &parent.warn_unused_cli);
        inherit_optional(&mut self.warn_system_vars, &parent.warn_system_vars);
        for (k, v) in &parent.cache_variables {
            self.cache_variables
                .entry(k.clone())
                .or_insert_with(|| v.clone());
        }
        true
    }

    fn visit_before_inherit(&self) -> bool {
        !self.base.environment.contains_key("")
    }

    fn visit_after_inherit(&self, version: i32, state: &mut State) -> bool {
        if !self.base.hidden {
            if version < 3 {
                if self.generator.is_empty() {
                    errors::preset_missing_field(&self.base.name, "generator", state);
                    return false;
                }
                if self.binary_dir.is_empty() {
                    errors::preset_missing_field(&self.base.name, "binaryDir", state);
                    return false;
                }
            }
            if self.warn_dev == Some(false) && self.error_dev == Some(true) {
                return false;
            }
            if self.warn_deprecated == Some(false) && self.error_deprecated == Some(true) {
                return false;
            }
            if self.cache_variables.contains_key("") {
                return false;
            }
        }
        true
    }

    fn expand_fields(
        graph: &Graph,
        preset: &Self,
        out: &mut Self,
        expanders: &[Expander],
    ) -> ExpandResult {
        let version = graph.version(preset);
        let mut binary_dir = preset.binary_dir.clone();
        check_expand!(binary_dir, expanders, version);
        if !binary_dir.is_empty() {
            if !crate::util::is_full_path(&binary_dir) {
                binary_dir = format!("{}/{binary_dir}", graph.source_dir);
            }
            out.binary_dir = crate::util::collapse_full_path(&binary_dir, "");
        }
        if !preset.install_dir.is_empty() {
            let mut install_dir = preset.install_dir.clone();
            check_expand!(install_dir, expanders, version);
            if !crate::util::is_full_path(&install_dir) {
                install_dir = format!("{}/{install_dir}", graph.source_dir);
            }
            out.install_dir = crate::util::collapse_full_path(&install_dir, "");
        }
        if !preset.toolchain_file.is_empty() {
            let mut toolchain = preset.toolchain_file.clone();
            check_expand!(toolchain, expanders, version);
            out.toolchain_file = toolchain;
        }
        if !preset.graphviz_file.is_empty() {
            let mut graphviz = preset.graphviz_file.clone();
            check_expand!(graphviz, expanders, version);
            out.graphviz_file = graphviz;
        }
        for variable in out.cache_variables.values_mut().flatten() {
            check_expand!(variable.value, expanders, version);
        }
        ExpandResult::Ok
    }
}

impl PresetKind for BuildPreset {
    fn base(&self) -> &Base {
        &self.base
    }
    fn base_mut(&mut self) -> &mut Base {
        &mut self.base
    }

    fn visit_inherit(&mut self, parent: &Self) -> bool {
        inherit_string(&mut self.configure_preset, &parent.configure_preset);
        inherit_optional(
            &mut self.inherit_configure_environment,
            &parent.inherit_configure_environment,
        );
        inherit_optional(&mut self.jobs, &parent.jobs);
        inherit_vector(&mut self.targets, &parent.targets);
        inherit_string(&mut self.configuration, &parent.configuration);
        inherit_optional(&mut self.clean_first, &parent.clean_first);
        inherit_optional(&mut self.verbose, &parent.verbose);
        inherit_vector(&mut self.native_tool_options, &parent.native_tool_options);
        inherit_optional(
            &mut self.resolve_package_references,
            &parent.resolve_package_references,
        );
        true
    }

    fn visit_after_inherit(&self, _version: i32, _state: &mut State) -> bool {
        self.base.hidden || !self.configure_preset.is_empty()
    }

    fn expand_fields(
        graph: &Graph,
        preset: &Self,
        out: &mut Self,
        expanders: &[Expander],
    ) -> ExpandResult {
        let version = graph.version(preset);
        for target in &mut out.targets {
            check_expand!(*target, expanders, version);
        }
        for option in &mut out.native_tool_options {
            check_expand!(*option, expanders, version);
        }
        ExpandResult::Ok
    }
}

impl PresetKind for TestPreset {
    fn base(&self) -> &Base {
        &self.base
    }
    fn base_mut(&mut self) -> &mut Base {
        &mut self.base
    }

    fn visit_inherit(&mut self, parent: &Self) -> bool {
        inherit_string(&mut self.configure_preset, &parent.configure_preset);
        inherit_optional(
            &mut self.inherit_configure_environment,
            &parent.inherit_configure_environment,
        );
        inherit_string(&mut self.configuration, &parent.configuration);
        inherit_vector(
            &mut self.overwrite_configuration_file,
            &parent.overwrite_configuration_file,
        );
        if let Some(parent_output) = &parent.output {
            if let Some(output) = &mut self.output {
                inherit_optional(&mut output.short_progress, &parent_output.short_progress);
                inherit_optional(&mut output.verbosity, &parent_output.verbosity);
                inherit_optional(&mut output.debug, &parent_output.debug);
                inherit_optional(
                    &mut output.output_on_failure,
                    &parent_output.output_on_failure,
                );
                inherit_optional(&mut output.quiet, &parent_output.quiet);
                inherit_string(&mut output.output_log_file, &parent_output.output_log_file);
                inherit_string(
                    &mut output.output_junit_file,
                    &parent_output.output_junit_file,
                );
                inherit_optional(&mut output.label_summary, &parent_output.label_summary);
                inherit_optional(
                    &mut output.subproject_summary,
                    &parent_output.subproject_summary,
                );
                inherit_optional(
                    &mut output.max_passed_test_output_size,
                    &parent_output.max_passed_test_output_size,
                );
                inherit_optional(
                    &mut output.max_failed_test_output_size,
                    &parent_output.max_failed_test_output_size,
                );
                inherit_optional(
                    &mut output.test_output_truncation,
                    &parent_output.test_output_truncation,
                );
                inherit_optional(
                    &mut output.max_test_name_width,
                    &parent_output.max_test_name_width,
                );
            } else {
                self.output = parent.output.clone();
            }
        }
        if let Some(parent_filter) = &parent.filter {
            if let Some(parent_include) = &parent_filter.include {
                if let Some(include) = self.filter.as_mut().and_then(|f| f.include.as_mut()) {
                    inherit_string(&mut include.name, &parent_include.name);
                    inherit_string(&mut include.label, &parent_include.label);
                    inherit_optional(&mut include.index, &parent_include.index);
                } else {
                    self.filter
                        .get_or_insert_with(FilterOptions::default)
                        .include = parent_filter.include.clone();
                }
            }
            if let Some(parent_exclude) = &parent_filter.exclude {
                if let Some(exclude) = self.filter.as_mut().and_then(|f| f.exclude.as_mut()) {
                    inherit_string(&mut exclude.name, &parent_exclude.name);
                    inherit_string(&mut exclude.label, &parent_exclude.label);
                    inherit_optional(&mut exclude.fixtures, &parent_exclude.fixtures);
                } else {
                    self.filter
                        .get_or_insert_with(FilterOptions::default)
                        .exclude = parent_filter.exclude.clone();
                }
            }
        }
        if let Some(parent_execution) = &parent.execution {
            if let Some(execution) = &mut self.execution {
                inherit_optional(
                    &mut execution.stop_on_failure,
                    &parent_execution.stop_on_failure,
                );
                inherit_optional(
                    &mut execution.enable_failover,
                    &parent_execution.enable_failover,
                );
                inherit_optional(&mut execution.jobs, &parent_execution.jobs);
                inherit_string(
                    &mut execution.resource_spec_file,
                    &parent_execution.resource_spec_file,
                );
                inherit_optional(&mut execution.test_load, &parent_execution.test_load);
                inherit_optional(&mut execution.show_only, &parent_execution.show_only);
                inherit_optional(&mut execution.repeat, &parent_execution.repeat);
                inherit_optional(
                    &mut execution.interactive_debugging,
                    &parent_execution.interactive_debugging,
                );
                inherit_optional(
                    &mut execution.schedule_random,
                    &parent_execution.schedule_random,
                );
                inherit_optional(&mut execution.timeout, &parent_execution.timeout);
                inherit_optional(
                    &mut execution.no_tests_action,
                    &parent_execution.no_tests_action,
                );
            } else {
                self.execution = parent.execution.clone();
            }
        }
        true
    }

    fn visit_after_inherit(&self, _version: i32, _state: &mut State) -> bool {
        self.base.hidden || !self.configure_preset.is_empty()
    }

    fn expand_fields(
        graph: &Graph,
        preset: &Self,
        out: &mut Self,
        expanders: &[Expander],
    ) -> ExpandResult {
        let version = graph.version(preset);
        for overwrite in &mut out.overwrite_configuration_file {
            check_expand!(*overwrite, expanders, version);
        }
        if let Some(output) = &mut out.output {
            check_expand!(output.output_log_file, expanders, version);
            check_expand!(output.output_junit_file, expanders, version);
        }
        if let Some(filter) = &mut out.filter {
            if let Some(include) = &mut filter.include {
                check_expand!(include.name, expanders, version);
                check_expand!(include.label, expanders, version);
                if let Some(index) = &mut include.index {
                    check_expand!(index.index_file, expanders, version);
                }
            }
            if let Some(exclude) = &mut filter.exclude {
                check_expand!(exclude.name, expanders, version);
                check_expand!(exclude.label, expanders, version);
                if let Some(fixtures) = &mut exclude.fixtures {
                    check_expand!(fixtures.any, expanders, version);
                    check_expand!(fixtures.setup, expanders, version);
                    check_expand!(fixtures.cleanup, expanders, version);
                }
            }
        }
        if let Some(execution) = &mut out.execution {
            check_expand!(execution.resource_spec_file, expanders, version);
        }
        ExpandResult::Ok
    }
}

impl PresetKind for PackagePreset {
    fn base(&self) -> &Base {
        &self.base
    }
    fn base_mut(&mut self) -> &mut Base {
        &mut self.base
    }

    fn visit_inherit(&mut self, parent: &Self) -> bool {
        inherit_string(&mut self.configure_preset, &parent.configure_preset);
        inherit_optional(
            &mut self.inherit_configure_environment,
            &parent.inherit_configure_environment,
        );
        inherit_vector(&mut self.generators, &parent.generators);
        inherit_vector(&mut self.configurations, &parent.configurations);
        for (k, v) in &parent.variables {
            self.variables.entry(k.clone()).or_insert_with(|| v.clone());
        }
        inherit_optional(&mut self.debug_output, &parent.debug_output);
        inherit_optional(&mut self.verbose_output, &parent.verbose_output);
        inherit_string(&mut self.package_name, &parent.package_name);
        inherit_string(&mut self.package_version, &parent.package_version);
        inherit_string(&mut self.package_directory, &parent.package_directory);
        inherit_string(&mut self.vendor_name, &parent.vendor_name);
        true
    }

    fn visit_after_inherit(&self, _version: i32, _state: &mut State) -> bool {
        self.base.hidden || !self.configure_preset.is_empty()
    }

    fn expand_fields(
        graph: &Graph,
        preset: &Self,
        out: &mut Self,
        expanders: &[Expander],
    ) -> ExpandResult {
        let version = graph.version(preset);
        for value in out.variables.values_mut() {
            check_expand!(*value, expanders, version);
        }
        check_expand!(out.config_file, expanders, version);
        check_expand!(out.package_name, expanders, version);
        check_expand!(out.package_version, expanders, version);
        check_expand!(out.package_directory, expanders, version);
        check_expand!(out.vendor_name, expanders, version);
        ExpandResult::Ok
    }
}

impl PresetKind for WorkflowPreset {
    fn base(&self) -> &Base {
        &self.base
    }
    fn base_mut(&mut self) -> &mut Base {
        &mut self.base
    }
    fn visit_inherit(&mut self, _parent: &Self) -> bool {
        true
    }
    fn expand_fields(
        _graph: &Graph,
        _preset: &Self,
        _out: &mut Self,
        _expanders: &[Expander],
    ) -> ExpandResult {
        ExpandResult::Ok
    }
}

// ---------------------------------------------------------------------
// Graph

#[derive(Clone, Copy, PartialEq, Eq)]
enum RootType {
    Project,
    User,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReadReason {
    Root,
    Included,
}

#[derive(Default)]
pub struct Graph {
    pub parse_state: State,
    pub configure_presets: BTreeMap<String, PresetPair<ConfigurePreset>>,
    pub build_presets: BTreeMap<String, PresetPair<BuildPreset>>,
    pub test_presets: BTreeMap<String, PresetPair<TestPreset>>,
    pub package_presets: BTreeMap<String, PresetPair<PackagePreset>>,
    pub workflow_presets: BTreeMap<String, PresetPair<WorkflowPreset>>,
    pub configure_order: Vec<String>,
    pub build_order: Vec<String>,
    pub test_order: Vec<String>,
    pub package_order: Vec<String>,
    pub workflow_order: Vec<String>,
    pub source_dir: String,
    pub files: Vec<File>,
}

/// `VisitPreset`; `cycle` is taken by value as in the C++.
fn visit_preset<T: PresetKind>(
    name: &str,
    presets: &mut BTreeMap<String, PresetPair<T>>,
    mut cycle: BTreeMap<String, CycleStatus>,
    files: &[File],
    state: &mut State,
) -> bool {
    match cycle.get(name).copied().unwrap_or(CycleStatus::Unvisited) {
        CycleStatus::InProgress => {
            errors::cyclic_preset_inheritance(name, state);
            return false;
        }
        CycleStatus::Verified => return true,
        CycleStatus::Unvisited => {}
    }
    cycle.insert(name.to_owned(), CycleStatus::InProgress);
    let mut preset = presets[name].unexpanded.clone();
    if preset.base().environment.contains_key("") {
        errors::invalid_preset_named(&preset.base().name, state);
        return false;
    }
    if !preset.visit_before_inherit() {
        errors::invalid_preset_named(&preset.base().name, state);
        return false;
    }
    for parent_name in preset.base().inherits.clone() {
        let Some(parent) = presets.get(&parent_name) else {
            errors::invalid_preset_named(&preset.base().name, state);
            return false;
        };
        let parent_file = parent.unexpanded.base().origin_file;
        if !files[preset.base().origin_file]
            .reachable
            .contains(&parent_file)
        {
            errors::inherited_preset_unreachable_from_file(&preset.base().name, state);
            return false;
        }
        if !visit_preset(&parent_name, presets, cycle.clone(), files, state) {
            return false;
        }
        let parent = presets[&parent_name].unexpanded.clone();
        if !preset.visit_inherit(&parent) {
            errors::invalid_preset_named(&preset.base().name, state);
            return false;
        }
        for (k, v) in &parent.base().environment {
            preset
                .base_mut()
                .environment
                .entry(k.clone())
                .or_insert_with(|| v.clone());
        }
        if preset.base().condition.is_none() {
            preset.base_mut().condition = parent.base().condition.clone();
        }
    }
    if preset
        .base()
        .condition
        .as_ref()
        .is_some_and(|c| c.is_null())
    {
        preset.base_mut().condition = None;
    }
    let version = files[preset.base().origin_file].version;
    if !preset.visit_after_inherit(version, state) {
        errors::invalid_preset_named(&preset.base().name, state);
        return false;
    }
    presets.get_mut(name).unwrap().unexpanded = preset;
    true
}

fn compute_preset_inheritance<T: PresetKind>(
    presets: &mut BTreeMap<String, PresetPair<T>>,
    files: &[File],
    state: &mut State,
) -> bool {
    let cycle: BTreeMap<String, CycleStatus> = presets
        .keys()
        .map(|k| (k.clone(), CycleStatus::Unvisited))
        .collect();
    let names: Vec<String> = presets.keys().cloned().collect();
    for name in names {
        if !visit_preset(&name, presets, cycle.clone(), files, state) {
            return false;
        }
    }
    true
}

impl Graph {
    pub fn version<T: PresetKind>(&self, preset: &T) -> i32 {
        self.files[preset.base().origin_file].version
    }

    pub fn filename(source_dir: &str) -> String {
        format!("{source_dir}/CMakePresets.json")
    }

    pub fn user_filename(source_dir: &str) -> String {
        format!("{source_dir}/CMakeUserPresets.json")
    }

    /// `GetGeneratorForPreset`.
    pub fn generator_for_preset(&self, name: &str) -> String {
        let mut configure_name = name.to_owned();
        if let Some(build) = self.build_presets.get(name) {
            configure_name = build.unexpanded.configure_preset.clone();
        } else if let Some(test) = self.test_presets.get(name) {
            configure_name = test.unexpanded.configure_preset.clone();
        }
        self.configure_presets
            .get(&configure_name)
            .map(|p| p.unexpanded.generator.clone())
            .unwrap_or_default()
    }

    fn clear_presets(&mut self) {
        self.configure_presets.clear();
        self.build_presets.clear();
        self.test_presets.clear();
        self.package_presets.clear();
        self.workflow_presets.clear();
        self.configure_order.clear();
        self.build_order.clear();
        self.test_order.clear();
        self.package_order.clear();
        self.workflow_order.clear();
        self.files.clear();
    }

    /// `ReadProjectPresets`.
    pub fn read_project_presets(&mut self, source_dir: &str, allow_no_files: bool) -> bool {
        self.source_dir = source_dir.to_owned();
        self.clear_presets();
        if !self.read_project_presets_internal(allow_no_files) {
            self.clear_presets();
            return false;
        }
        true
    }

    fn read_project_presets_internal(&mut self, allow_no_files: bool) -> bool {
        let mut have_one_file = false;
        let mut in_progress = Vec::new();
        let mut file = 0;
        let mut filename = Self::user_filename(&self.source_dir);
        if std::path::Path::new(&filename).exists() {
            if !self.read_json_file(
                &filename,
                RootType::User,
                ReadReason::Root,
                &mut in_progress,
                &mut file,
            ) {
                return false;
            }
            have_one_file = true;
        } else {
            filename = Self::filename(&self.source_dir);
            if std::path::Path::new(&filename).exists() {
                if !self.read_json_file(
                    &filename,
                    RootType::Project,
                    ReadReason::Root,
                    &mut in_progress,
                    &mut file,
                ) {
                    return false;
                }
                have_one_file = true;
            }
        }
        if !have_one_file {
            if allow_no_files {
                return true;
            }
            errors::file_not_found(&filename, &mut self.parse_state);
            return false;
        }

        let files = std::mem::take(&mut self.files);
        let ok =
            compute_preset_inheritance(&mut self.configure_presets, &files, &mut self.parse_state)
                && compute_preset_inheritance(
                    &mut self.build_presets,
                    &files,
                    &mut self.parse_state,
                )
                && compute_preset_inheritance(
                    &mut self.test_presets,
                    &files,
                    &mut self.parse_state,
                )
                && compute_preset_inheritance(
                    &mut self.package_presets,
                    &files,
                    &mut self.parse_state,
                )
                && compute_preset_inheritance(
                    &mut self.workflow_presets,
                    &files,
                    &mut self.parse_state,
                );
        self.files = files;
        if !ok {
            return false;
        }

        let names: Vec<String> = self.configure_presets.keys().cloned().collect();
        for name in names {
            let preset = self.configure_presets[&name].unexpanded.clone();
            let expanded = self.expand_preset(&preset);
            if !self.store_expansion(&name, expanded, |g| &mut g.configure_presets) {
                return false;
            }
        }

        macro_rules! dependent_presets {
            ($map:ident) => {
                let names: Vec<String> = self.$map.keys().cloned().collect();
                for name in names {
                    let pair = &self.$map[&name];
                    if !pair.unexpanded.base.hidden {
                        let configure_name = pair.unexpanded.configure_preset.clone();
                        let origin = pair.unexpanded.base.origin_file;
                        let Some(configure) = self.configure_presets.get(&configure_name) else {
                            errors::invalid_configure_preset(&name, &mut self.parse_state);
                            return false;
                        };
                        if !self.files[origin]
                            .reachable
                            .contains(&configure.unexpanded.base.origin_file)
                        {
                            errors::configure_preset_unreachable_from_file(
                                &name,
                                &mut self.parse_state,
                            );
                            return false;
                        }
                        let configure_env = configure.unexpanded.base.environment.clone();
                        let pair = self.$map.get_mut(&name).unwrap();
                        if pair
                            .unexpanded
                            .inherit_configure_environment
                            .unwrap_or(true)
                        {
                            for (k, v) in configure_env {
                                pair.unexpanded.base.environment.entry(k).or_insert(v);
                            }
                        }
                    }
                    let preset = self.$map[&name].unexpanded.clone();
                    let expanded = self.expand_preset(&preset);
                    if !self.store_expansion(&name, expanded, |g| &mut g.$map) {
                        return false;
                    }
                }
            };
        }
        dependent_presets!(build_presets);
        dependent_presets!(test_presets);
        dependent_presets!(package_presets);

        let names: Vec<String> = self.workflow_presets.keys().cloned().collect();
        for name in names {
            let workflow = self.workflow_presets[&name].unexpanded.clone();
            let mut configure_preset: Option<String> = None;
            for step in &workflow.steps {
                if configure_preset.is_none() && step.preset_type != StepType::Configure {
                    errors::first_workflow_step_not_configure(
                        &step.preset_name,
                        &mut self.parse_state,
                    );
                    return false;
                }
                if configure_preset.is_some() && step.preset_type == StepType::Configure {
                    errors::configure_workflow_step_not_first(
                        &step.preset_name,
                        &mut self.parse_state,
                    );
                    return false;
                }
                let found = match step.preset_type {
                    StepType::Configure => self
                        .configure_presets
                        .get(&step.preset_name)
                        .map(|p| (p.unexpanded.base.origin_file, None)),
                    StepType::Build => self.build_presets.get(&step.preset_name).map(|p| {
                        (
                            p.unexpanded.base.origin_file,
                            Some(p.unexpanded.configure_preset.clone()),
                        )
                    }),
                    StepType::Test => self.test_presets.get(&step.preset_name).map(|p| {
                        (
                            p.unexpanded.base.origin_file,
                            Some(p.unexpanded.configure_preset.clone()),
                        )
                    }),
                    StepType::Package => self.package_presets.get(&step.preset_name).map(|p| {
                        (
                            p.unexpanded.base.origin_file,
                            Some(p.unexpanded.configure_preset.clone()),
                        )
                    }),
                };
                // TryReachPresetFromWorkflow
                let Some((origin, step_configure)) = found else {
                    errors::invalid_workflow_steps(&step.preset_name, &mut self.parse_state);
                    return false;
                };
                if !self.files[workflow.base.origin_file]
                    .reachable
                    .contains(&origin)
                {
                    errors::workflow_step_unreachable_from_file(
                        &step.preset_name,
                        &mut self.parse_state,
                    );
                    return false;
                }
                match step_configure {
                    None => configure_preset = Some(step.preset_name.clone()),
                    Some(step_configure) => {
                        let current = configure_preset.clone().unwrap_or_default();
                        if step_configure != current {
                            errors::invalid_workflow_steps(&current, &mut self.parse_state);
                            return false;
                        }
                    }
                }
            }
            if configure_preset.is_none() {
                errors::no_workflow_steps(&name, &mut self.parse_state);
                return false;
            }
            let expanded = self.expand_preset(&workflow);
            if !self.store_expansion(&name, expanded, |g| &mut g.workflow_presets) {
                return false;
            }
        }
        true
    }

    fn store_expansion<T: PresetKind>(
        &mut self,
        name: &str,
        expanded: Result<Option<T>, bool>,
        map: fn(&mut Graph) -> &mut BTreeMap<String, PresetPair<T>>,
    ) -> bool {
        match expanded {
            Ok(expanded) => {
                map(self).get_mut(name).unwrap().expanded = expanded;
                true
            }
            Err(named) => {
                if named {
                    errors::invalid_preset_named(name, &mut self.parse_state);
                }
                errors::invalid_macro_expansion(name, &mut self.parse_state);
                false
            }
        }
    }

    /// The generic `ExpandMacros(graph, preset, out)`: `Err(true)` when an
    /// `INVALID_PRESET_NAMED` error precedes the failure.
    fn expand_preset<T: PresetKind>(&self, preset: &T) -> Result<Option<T>, bool> {
        let mut out = preset.clone();
        let version = self.version(preset);
        let ctx = RefCell::new(EnvContext {
            env: out.base().environment.clone(),
            cycles: out
                .base()
                .environment
                .keys()
                .map(|k| (k.clone(), CycleStatus::Unvisited))
                .collect(),
        });
        let expanders = [
            Expander::Base {
                source_dir: &self.source_dir,
                file: None,
            },
            Expander::Preset {
                name: preset.base().name.clone(),
                hidden: preset.base().hidden,
                generator: self.generator_for_preset(&preset.base().name),
                origin_file: self.files[preset.base().origin_file].filename.clone(),
            },
            Expander::Environment(&ctx),
        ];
        let keys: Vec<String> = ctx.borrow().env.keys().cloned().collect();
        for key in keys {
            let current = ctx.borrow().env.get(&key).cloned().flatten();
            if let Some(mut value) = current {
                match visit_env(&mut value, &key, &ctx, &expanders, version) {
                    ExpandResult::Error => return Err(true),
                    ExpandResult::Ignore => return Ok(None),
                    ExpandResult::Ok => {
                        ctx.borrow_mut().env.insert(key.clone(), Some(value));
                    }
                }
            }
        }
        if let Some(condition) = &preset.base().condition {
            match condition.evaluate(&expanders, version) {
                Err(()) => return Err(true),
                Ok(None) => return Ok(None),
                Ok(Some(result)) => out.base_mut().condition_result = result,
            }
        }
        let result = T::expand_fields(self, preset, &mut out, &expanders);
        out.base_mut().environment = ctx.borrow().env.clone();
        match result {
            ExpandResult::Ok => Ok(Some(out)),
            ExpandResult::Ignore => Ok(None),
            ExpandResult::Error => Err(false),
        }
    }

    // -----------------------------------------------------------------
    // Printing

    fn print_presets(presets: &[&Base]) {
        if presets.is_empty() {
            return;
        }
        let longest = presets.iter().map(|p| p.name.len()).max().unwrap_or(0);
        let mut out = String::new();
        for preset in presets {
            out += &format!("  \"{}\"", preset.name);
            if !preset.display_name.is_empty() {
                out += &" ".repeat(longest - preset.name.len());
                out += &format!(" - {}", preset.display_name);
            }
            out.push('\n');
        }
        print!("{out}");
    }

    fn print_list<T: PresetKind>(
        order: &[String],
        presets: &BTreeMap<String, PresetPair<T>>,
        filter: &dyn Fn(&T) -> bool,
        kind: &str,
        newline: Option<&mut bool>,
    ) {
        let list: Vec<&Base> = order
            .iter()
            .map(|name| &presets[name])
            .filter(|p| {
                !p.unexpanded.base().hidden
                    && p.expanded
                        .as_ref()
                        .is_some_and(|e| e.base().condition_result)
                    && filter(&p.unexpanded)
            })
            .map(|p| p.unexpanded.base())
            .collect();
        if !list.is_empty() {
            if let Some(newline) = newline {
                if *newline {
                    println!();
                }
                *newline = true;
            }
            print!("Available {kind} presets:\n\n");
            Self::print_presets(&list);
        }
    }

    pub fn print_configure_preset_list(
        &self,
        filter: &dyn Fn(&ConfigurePreset) -> bool,
        newline: Option<&mut bool>,
    ) {
        Self::print_list(
            &self.configure_order,
            &self.configure_presets,
            filter,
            "configure",
            newline,
        );
    }

    pub fn print_build_preset_list(&self, newline: Option<&mut bool>) {
        Self::print_list(
            &self.build_order,
            &self.build_presets,
            &|_| true,
            "build",
            newline,
        );
    }

    pub fn print_test_preset_list(&self, newline: Option<&mut bool>) {
        Self::print_list(
            &self.test_order,
            &self.test_presets,
            &|_| true,
            "test",
            newline,
        );
    }

    pub fn print_package_preset_list(&self, newline: Option<&mut bool>) {
        Self::print_list(
            &self.package_order,
            &self.package_presets,
            &|_| true,
            "package",
            newline,
        );
    }

    pub fn print_workflow_preset_list(&self, newline: Option<&mut bool>) {
        Self::print_list(
            &self.workflow_order,
            &self.workflow_presets,
            &|_| true,
            "workflow",
            newline,
        );
    }

    /// `PrintAllPresets`; the configure list uses no generator filter.
    pub fn print_all_presets(&self) {
        let mut newline = false;
        self.print_configure_preset_list(&|_| true, Some(&mut newline));
        self.print_build_preset_list(Some(&mut newline));
        self.print_test_preset_list(Some(&mut newline));
        self.print_package_preset_list(Some(&mut newline));
        self.print_workflow_preset_list(Some(&mut newline));
    }
}
