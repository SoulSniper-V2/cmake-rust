//! Build-system state that commands share: targets, tests, and auxiliary
//! interpreter extension state.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::policy::{PolicyMap, PolicyStatus};

use crate::interp::Interp;
use crate::messages::Frame;
use crate::props::Props;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetKind {
    Executable,
    StaticLibrary,
    SharedLibrary,
    ModuleLibrary,
    ObjectLibrary,
    InterfaceLibrary,
    UnknownLibrary,
    Utility,
    GlobalTarget,
}

impl TargetKind {
    /// Value of the TYPE property.
    pub fn type_name(self) -> &'static str {
        match self {
            TargetKind::Executable => "EXECUTABLE",
            TargetKind::StaticLibrary => "STATIC_LIBRARY",
            TargetKind::SharedLibrary => "SHARED_LIBRARY",
            TargetKind::ModuleLibrary => "MODULE_LIBRARY",
            TargetKind::ObjectLibrary => "OBJECT_LIBRARY",
            TargetKind::InterfaceLibrary => "INTERFACE_LIBRARY",
            TargetKind::UnknownLibrary => "UNKNOWN_LIBRARY",
            TargetKind::Utility => "UTILITY",
            TargetKind::GlobalTarget => "GLOBAL_TARGET",
        }
    }
}

/// A target (`cmTarget`). Property values that carry backtraces keep them in
/// `provenance` (for usage requirements and `--explain`).
#[derive(Debug)]
pub struct Target {
    pub name: String,
    pub kind: TargetKind,
    pub makefile: usize,
    pub imported: bool,
    pub imported_global: bool,
    #[allow(dead_code, reason = "legacy target model field used by integration")]
    pub alias_of: Option<String>,
    pub props: Props,
    /// Backtraces for list-valued properties, parallel to entries appended.
    pub provenance: BTreeMap<String, Vec<PropertyEntry>>,
    #[allow(dead_code, reason = "target diagnostics in the generator")]
    pub backtrace: Vec<Frame>,
    pub policies: PolicyMap,
    pub system_include_directories: BTreeSet<String>,
    pub dependencies: BTreeMap<String, Vec<Frame>>,
    pub custom_commands: Vec<CustomCommand>,
    pub file_sets: BTreeMap<String, FileSet>,
    pub(super) link_signatures: Vec<(bool, Frame)>,
}

pub type PropertyEntry = (String, Vec<Frame>);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Scope {
    Private,
    Public,
    Interface,
}

impl Scope {
    pub fn direct(self) -> bool {
        self != Self::Interface
    }

    pub fn interface(self) -> bool {
        self != Self::Private
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Private => "PRIVATE",
            Self::Public => "PUBLIC",
            Self::Interface => "INTERFACE",
        }
    }
}

#[derive(Debug)]
pub struct FileSet {
    pub kind: String,
    pub scope: Scope,
    pub base_dirs: Vec<PropertyEntry>,
    pub files: Vec<PropertyEntry>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CustomCommandKind {
    #[default]
    Output,
    PreBuild,
    PreLink,
    PostBuild,
    Utility,
}

#[derive(Debug, Default)]
pub struct CustomCommand {
    pub kind: CustomCommandKind,
    pub makefile: usize,
    pub order: usize,
    pub command_lines: Vec<Vec<String>>,
    pub outputs: Vec<String>,
    pub byproducts: Vec<String>,
    pub depends: Vec<String>,
    pub implicit_depends: Vec<(String, String)>,
    pub main_dependency: String,
    pub working_directory: String,
    pub comment: Option<String>,
    pub depfile: String,
    pub job_pool: String,
    pub job_server_aware: bool,
    pub verbatim: bool,
    pub uses_terminal: bool,
    pub command_expand_lists: bool,
    pub depends_explicit_only: bool,
    pub codegen: bool,
    #[allow(dead_code, reason = "custom command generation metadata")]
    pub backtrace: Vec<Frame>,
    #[allow(dead_code, reason = "custom command generation metadata")]
    pub policies: PolicyMap,
}

#[derive(Debug)]
pub struct SourceFile {
    #[allow(
        dead_code,
        reason = "source file directory scope for property integration"
    )]
    pub makefile: usize,
    pub props: Props,
    pub location: SourceFileLocation,
    pub generated: bool,
    pub custom_command: Option<CustomCommand>,
}

#[derive(Debug)]
pub struct SourceFileLocation {
    pub directory: String,
    pub name: String,
    pub ambiguous_directory: bool,
    pub ambiguous_extension: bool,
}

#[derive(Debug)]
pub struct SourceGroup {
    pub regex: Option<String>,
    pub files: BTreeSet<String>,
}

#[derive(Debug)]
#[allow(dead_code, reason = "test model also carries generation metadata")]
pub struct Test {
    pub name: String,
    pub makefile: usize,
    pub command: Vec<String>,
    pub props: Props,
    pub old_style: bool,
    pub configurations: Vec<String>,
    pub command_expand_lists: bool,
    pub empty_argument_policy: PolicyStatus,
    pub emulator_policy: PolicyStatus,
    pub backtrace: Vec<Frame>,
}

/// Extension state for command implementations.
#[derive(Debug, Default)]
pub struct Ext {
    pub targets: HashMap<String, Target>,
    /// Target names in creation order.
    pub target_order: Vec<String>,
    /// Alias name -> real target name.
    pub aliases: HashMap<String, String>,
    pub tests: Vec<Test>,
    pub testing_enabled: BTreeSet<usize>,
    pub sources: BTreeMap<usize, Vec<SourceFile>>,
    pub generated_sources: BTreeSet<String>,
    pub source_groups: BTreeMap<usize, BTreeMap<String, SourceGroup>>,
    pub directory_provenance: BTreeMap<usize, BTreeMap<String, Vec<PropertyEntry>>>,
    pub directory_system_includes: BTreeMap<usize, BTreeSet<String>>,
    pub directory_define_flags: BTreeMap<usize, String>,
    pub pending_subdirs: BTreeMap<usize, Vec<PendingSubdirectory>>,
    pub pending_custom_appends: Vec<CustomCommand>,
    pub custom_command_count: usize,
    pub scoped_targets: HashMap<(usize, String), String>,
    pub scoped_aliases: HashMap<(usize, String), String>,
    pub target_property_initializers: BTreeMap<String, String>,
    pub defer_current_id: Option<String>,
    /// Enabled languages.
    pub languages: Vec<String>,
    /// Free-form registry for command modules that need global state.
    pub misc: HashMap<String, String>,
}

#[derive(Debug)]
pub struct PendingSubdirectory {
    pub source: String,
    pub binary: String,
    pub exclude_from_all: bool,
    pub backtrace: Vec<Frame>,
}

/// Look up a visible alias and return its target key and global visibility.
/// A local alias stays local even if its imported target is promoted later.
pub fn find_alias<'a>(interp: &'a Interp, name: &str) -> Option<(&'a str, bool)> {
    find_alias_at(interp, interp.mf_index(), name)
}

fn find_alias_at<'a>(interp: &'a Interp, dir: usize, name: &str) -> Option<(&'a str, bool)> {
    if let Some(key) = interp.ext.scoped_aliases.get(&(dir, name.to_owned())) {
        return Some((key, false));
    }
    interp.ext.aliases.get(name).map(|key| (key.as_str(), true))
}

/// Target keys index both `targets` and `target_order`.
pub fn find_target_key(interp: &Interp, name: &str) -> Option<String> {
    find_target_key_at(interp, interp.mf_index(), name)
}

/// Resolve a link item in the directory encoded by target_link_libraries.
pub fn find_target_key_at(interp: &Interp, dir: usize, name: &str) -> Option<String> {
    if let Some((key, false)) = find_alias_at(interp, dir, name) {
        return Some(key.to_owned());
    }
    if let Some(key) = interp.ext.scoped_targets.get(&(dir, name.to_owned())) {
        return Some(key.clone());
    }
    find_alias_at(interp, dir, name)
        .map(|(key, _)| key.to_owned())
        .or_else(|| find_global_target_key(interp, name))
}

pub fn find_global_target_key(interp: &Interp, name: &str) -> Option<String> {
    if interp
        .ext
        .targets
        .get(name)
        .is_some_and(|target| !target.imported || target.imported_global)
    {
        return Some(name.to_owned());
    }
    interp
        .ext
        .target_order
        .iter()
        .find(|key| {
            let target = &interp.ext.targets[*key];
            target.name == name && target.imported_global
        })
        .cloned()
}

/// `cmMakefile::FindTargetToUse`.
pub fn find_target_to_use<'a>(interp: &'a Interp, name: &str) -> Option<&'a Target> {
    interp.ext.targets.get(&find_target_key(interp, name)?)
}

#[allow(unused_imports, reason = "shared API for property command integration")]
pub use crate::commands::target::{
    PropertyWrite, find_source, get_or_create_source, mark_generated, read_target_property,
    target_property, write_target_property,
};

pub fn has_test(interp: &Interp, name: &str) -> bool {
    let current = interp.mf_index();
    interp
        .ext
        .tests
        .iter()
        .any(|test| test.makefile == current && test.name == name)
}

/// `cmMakefile::ClearMatches`.
pub fn clear_matches(interp: &mut Interp) {
    let Some(count) = interp.get_def("CMAKE_MATCH_COUNT") else {
        return;
    };
    let count = crate::util::strtol(count, 10).0.clamp(0, 9) as usize;
    for index in 0..=count {
        let name = format!("CMAKE_MATCH_{index}");
        if interp.get_def(&name).is_some_and(|value| !value.is_empty()) {
            interp.set_def(name, "");
        }
    }
    interp.set_def("CMAKE_MATCH_COUNT", "0");
}

/// `cmMakefile::StoreMatches`: only non-empty groups are stored.
pub fn store_matches(interp: &mut Interp, text: &str, found: &crate::cmregex::Match) {
    // Mirrors the C++ quirk: the count is empty when no group matched text.
    let mut highest: Option<usize> = None;
    for index in 0..10 {
        let value = found.groups[index]
            .map(|(start, end)| text.get(start..end).unwrap_or("").to_owned())
            .unwrap_or_default();
        if !value.is_empty() {
            interp.set_def(format!("CMAKE_MATCH_{index}"), value);
            highest = Some(index);
        }
    }
    interp.set_def(
        "CMAKE_MATCH_COUNT",
        highest.map(|index| index.to_string()).unwrap_or_default(),
    );
}

/// `cmCMakePath ==` comparison (lexical, component-wise).
pub fn path_equal(lhs: &str, rhs: &str) -> bool {
    fn components(path: &str) -> Vec<&str> {
        let mut parts = Vec::new();
        if path.starts_with('/') {
            parts.push("/");
        }
        for part in path.split('/') {
            if !part.is_empty() {
                parts.push(part);
            }
        }
        if path.len() > 1 && path.ends_with('/') {
            parts.push("");
        }
        parts
    }
    components(lhs) == components(rhs)
}
