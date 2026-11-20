//! Build-system state that commands share: targets, tests, and auxiliary
//! interpreter extension state.

use std::collections::BTreeMap;
use std::collections::HashMap;

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
/// `entries` (for usage requirements and `--explain`).
#[derive(Debug)]
pub struct Target {
    pub name: String,
    pub kind: TargetKind,
    pub makefile: usize,
    pub imported: bool,
    pub imported_global: bool,
    pub alias_of: Option<String>,
    pub props: Props,
    /// Backtraces for list-valued properties, parallel to entries appended.
    pub provenance: BTreeMap<String, Vec<(String, Vec<Frame>)>>,
    pub backtrace: Vec<Frame>,
}

#[derive(Debug)]
pub struct Test {
    pub name: String,
    pub makefile: usize,
    pub command: Vec<String>,
    pub props: Props,
    pub old_style: bool,
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
    pub execution_files: Vec<String>,
    pub defer_current_id: Option<String>,
    pub dependency_provider: Option<(String, Vec<String>)>,
    pub find_package: HashMap<usize, crate::commands::find::package::PackageState>,
    pub next_defer_id: usize,
    pub experimental_warnings: std::collections::HashSet<(String, String)>,
    pub in_top_level_includes: bool,
    pub message_checks: Vec<String>,

    pub property_definitions: crate::properties::Definitions,
    pub source_properties: HashMap<usize, Vec<crate::properties::SourceFile>>,
    pub generated_sources: std::collections::HashSet<String>,
    pub install_properties: HashMap<String, Props>,
    pub directory_definitions: HashMap<usize, BTreeMap<String, String>>,
    pub variable_watches: HashMap<String, Vec<String>>,
    pub active_variable_watches: std::collections::HashSet<(String, usize)>,
    pub host_information: Option<crate::commands::sysinfo::HostInformation>,
    pub distribution: Option<BTreeMap<String, String>>,
    pub file_api_queries: BTreeMap<String, (u32, u32)>,
    /// Enabled languages.
    pub languages: Vec<String>,
    /// Free-form registry for command modules that need global state.
    pub misc: HashMap<String, String>,
    /// `cmMakefile::GetOrCreateConfigureLog` state (created lazily).
    pub configure_log: std::cell::RefCell<Option<crate::configure_log::ConfigureLog>>,
}

/// `cmMakefile::FindTargetToUse`: aliases, then targets visible from the
/// current directory (imported non-global targets are directory-scoped).
pub fn find_target_to_use<'a>(interp: &'a Interp, name: &str) -> Option<&'a Target> {
    let real = interp.ext.aliases.get(name).map(String::as_str).unwrap_or(name);
    let target = interp.ext.targets.get(real)?;
    if target.imported && !target.imported_global {
        // Imported targets are visible in the directory that created them and
        // its subdirectories.
        let mut directory = Some(interp.mf_index());
        while let Some(index) = directory {
            if index == target.makefile {
                return Some(target);
            }
            directory = interp.makefiles[index].parent;
        }
        return None;
    }
    Some(target)
}

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
