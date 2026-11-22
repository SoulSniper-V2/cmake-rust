//! Configure-time state shared by install() and export().
use std::collections::{BTreeMap, HashMap};

use crate::messages::Frame;

#[derive(Debug, Default)]
pub struct InstallState {
    pub enabled: bool,
    pub rules: HashMap<usize, Vec<InstallRule>>,
    pub export_sets: HashMap<String, ExportSet>,
    pub build_exports: BTreeMap<String, BuildExport>,
    pub runtime_sets: HashMap<String, Vec<String>>,
}

#[derive(Clone, Debug, Default)]
pub struct InstallOptions {
    pub destination: String,
    pub component: String,
    pub namelink_component: String,
    pub configurations: Vec<String>,
    pub permissions: Vec<String>,
    pub rename: String,
    pub optional: bool,
    pub exclude: bool,
    pub namelink_only: bool,
    pub namelink_skip: bool,
    pub message: String,
}

#[derive(Debug)]
pub struct InstallRule {
    pub options: InstallOptions,
    pub action: InstallAction,
    pub preceding_children: usize,
    pub backtrace: Vec<Frame>,
}

#[derive(Debug)]
pub enum InstallAction {
    Copy {
        kind: CopyKind,
        paths: Vec<String>,
        dir_permissions: Vec<String>,
        arguments: String,
    },
    Script {
        text: String,
        is_code: bool,
        all_components: bool,
    },
    Target {
        name: String,
        namelink: Namelink,
        imported: bool,
    },
    FileSet { name: String, set: String },
    Bmi(String),
    Export(InstallExport),
    RuntimeDependencies {
        set: String,
        filters: BTreeMap<String, Vec<String>>,
    },
    LegacyFiles {
        args: Vec<String>,
        programs: bool,
    },
    LegacyTarget(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Namelink { None, Only, Skip }

#[derive(Clone, Copy, Debug)]
pub enum CopyKind { File, Program, Directory, Executable, Static, Shared, Module }

impl CopyKind {
    pub fn token(self) -> &'static str {
        match self {
            Self::File => "FILE",
            Self::Program => "PROGRAM",
            Self::Directory => "DIRECTORY",
            Self::Executable => "EXECUTABLE",
            Self::Static => "STATIC_LIBRARY",
            Self::Shared => "SHARED_LIBRARY",
            Self::Module => "MODULE",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ExportOptions {
    pub namespace: String,
    pub modules_directory: String,
    pub old_link_interface: bool,
    pub package_dependencies: bool,
}

#[derive(Debug)]
pub struct InstallExport {
    pub set: String,
    pub filename: String,
    pub options: ExportOptions,
}

#[derive(Debug)]
pub struct BuildExport {
    pub makefile: usize,
    pub set: Option<String>,
    pub targets: Vec<String>,
    pub options: ExportOptions,
    pub append: bool,
    pub backtrace: Vec<Frame>,
}

#[derive(Debug, Default)]
pub struct ExportSet {
    pub targets: Vec<ExportTarget>,
    pub xcframework_locations: HashMap<String, String>,
    pub package_dependencies: Vec<(String, String, Vec<String>)>,
}

#[derive(Debug)]
pub struct ExportTarget {
    pub name: String,
    pub artifact: Option<InstallOptions>,
    pub includes: Vec<String>,
    pub file_sets: BTreeMap<String, String>,
    /// Destination of the namelink (for `NAMELINK_ONLY` exports).
    pub namelink: Option<InstallOptions>,
    pub namelink_only: bool,
}
