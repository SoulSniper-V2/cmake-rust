//! Configure-time state shared by install() and export().
use std::collections::{BTreeMap, HashMap};

use crate::messages::Frame;

#[derive(Debug, Default)]
pub struct InstallState {
    pub enabled: bool,
    pub rules: BTreeMap<usize, Vec<InstallRule>>,
    /// `cmExportSetMap` (a std::map: iteration in name order).
    pub export_sets: BTreeMap<String, ExportSet>,
    pub build_exports: BTreeMap<String, BuildExport>,
    /// `cmInstallRuntimeDependencySet` contents per set name.
    pub runtime_sets: HashMap<String, RuntimeDependencySet>,
}

/// `cmInstallRuntimeDependencySet`'s item lists: the (directory, name) of
/// each target added to the set.
#[derive(Debug, Default)]
pub struct RuntimeDependencySet {
    pub executables: Vec<(usize, String)>,
    pub libraries: Vec<(usize, String)>,
    pub modules: Vec<(usize, String)>,
    /// The single bundle executable (also listed in `executables`).
    pub bundle_executable: Option<(usize, String)>,
}

/// `cmInstallRuntimeDependencySetGenerator::DependencyType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DepSetKind {
    Library,
    Framework,
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
    /// `cmInstallTargetGenerator`.
    Target {
        name: String,
        namelink: Namelink,
        /// `ImportlinkMode`: the namelink mode of an import library.
        importlink: Namelink,
        /// `ImportLibrary`: install the import library artifact.
        implib: bool,
        /// `cmInstallImportedRuntimeArtifactsGenerator`.
        imported: bool,
    },
    FileSet {
        name: String,
        set: String,
    },
    Bmi(String),
    Export(InstallExport),
    /// `install(PACKAGE_INFO ... EXPORT <set>)`.
    PackageInfo {
        set: String,
        arguments: crate::package_info::PackageInfoArguments,
    },
    /// `cmInstallGetRuntimeDependenciesGenerator`.
    RuntimeDependencies {
        set: String,
        filters: BTreeMap<String, Vec<String>>,
        /// `LibraryComponent` for the component test (the RUNTIME group on
        /// DLL platforms, the LIBRARY group elsewhere).
        library_component: String,
        /// `FrameworkComponent` for the component test; empty except on a
        /// Darwin host, where it is the FRAMEWORK group's component.
        framework_component: String,
        /// Library-group configurations plus, on a Darwin host, the
        /// framework group's.
        configurations: Vec<String>,
        /// `library.exclude_from_all && (apple ? framework.exclude : true)`.
        exclude_from_all: bool,
        /// Whether the framework group is EXCLUDE_FROM_ALL.
        framework_exclude: bool,
    },
    /// `cmInstallRuntimeDependencySetGenerator`: one per installable
    /// dependency kind.
    RuntimeDepSet {
        set: String,
        kind: DepSetKind,
        /// The resolved destination of the group that owns this generator.
        destination: String,
        component: String,
        configurations: Vec<String>,
        permissions: Vec<String>,
        exclude_from_all: bool,
    },
    LegacyFiles {
        args: Vec<String>,
        programs: bool,
    },
    LegacyTarget(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Namelink {
    None,
    Only,
    Skip,
}

#[derive(Clone, Copy, Debug)]
pub enum CopyKind {
    File,
    Program,
    Directory,
    Executable,
    Static,
    Shared,
    Module,
}

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
    /// `cmExport*AndroidMKGenerator`: write an NDK `Android.mk` instead.
    pub android: bool,
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
    /// `export(EXPORT <set> PACKAGE_INFO <name> ...)` → CPS output.
    pub package_info: Option<crate::package_info::PackageInfoArguments>,
    pub backtrace: Vec<Frame>,
}

#[derive(Debug, Default)]
pub struct ExportSet {
    pub targets: Vec<ExportTarget>,
    pub xcframework_locations: HashMap<String, String>,
    pub package_dependencies: Vec<(String, String, Vec<String>)>,
}

/// `cmTargetExport`: the install generators of an exported target (only
/// each generator's `Destination` is consumed by the export writers).
#[derive(Debug)]
pub struct ExportTarget {
    pub name: String,
    /// `ArchiveGenerator` (static libraries and import libraries).
    pub archive: Option<InstallOptions>,
    /// `RuntimeGenerator` (executables and DLLs).
    pub runtime: Option<InstallOptions>,
    /// `LibraryGenerator` (shared and module libraries).
    pub library: Option<InstallOptions>,
    /// `ObjectsGenerator` (object libraries).
    pub objects: Option<InstallOptions>,
    /// `FrameworkGenerator` (Apple frameworks).
    pub framework: Option<InstallOptions>,
    /// `BundleGenerator` (Apple application bundles).
    pub bundle: Option<InstallOptions>,
    /// `NamelinkOnly`: the target has no artifacts to export because a
    /// namelink-only install rule selected it.
    pub namelink_only: bool,
    /// `cmInstallCxxModuleBmiGenerator::GetCxxModuleDirectory`.
    pub cxx_module_dir: String,
    pub includes: Vec<String>,
    /// File set name -> its install generator's destination.
    pub file_sets: BTreeMap<String, String>,
}
