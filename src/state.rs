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
    /// `target_link_libraries` signature uses (keyword?, call site) for the
    /// plain/keyword signature mixing check.
    pub link_signatures: Vec<(bool, Frame)>,
    /// `cmTarget::GetHaveInstallRule` (set by install(TARGETS)).
    pub have_install_rule: bool,
    /// `cmTarget::AddUtility` from dependency tracing (name, cross).
    pub traced_utilities: Vec<(String, bool)>,
    /// `cmTarget::AddCodegenDependency`.
    pub codegen_dependencies: BTreeSet<String>,
    /// `cmGeneratorTarget::UnityBatchedSourceFiles`.
    pub unity_batched: BTreeSet<String>,
    /// `cmTarget::GetFindPackageStack` (enclosing find_package calls).
    pub find_package_stack: Vec<(String, u32)>,
    /// `cmTarget::IsSynthetic`: a BMI-only C++ module target synthesized by
    /// the generator from an IMPORTED target (`Visibility::Generated`).
    pub synthetic: bool,
    /// `cmTarget::TemplateName`: the imported target this synthetic target
    /// was created from (`GetTemplateName` falls back to `Name`).
    pub template_name: Option<String>,
    /// `cmTarget::IsGeneratorProvided`.
    pub generator_provided: bool,
    pub platform: Platform,
}

/// The platform a target is created for (`cmTargetInternals::IsDLLPlatform`,
/// `IsAIX`, `IsApple` and `IsAndroid`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Platform {
    pub dll: bool,
    pub aix: bool,
    pub apple: bool,
    pub android: bool,
}

impl Platform {
    /// Reads the platform from variable definitions, as the cmTarget
    /// constructor does.
    pub fn new<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Self {
        let system = get("CMAKE_SYSTEM_NAME").unwrap_or_default();
        Platform {
            dll: get("CMAKE_IMPORT_LIBRARY_SUFFIX").is_some_and(|suffix| !suffix.is_empty()),
            aix: matches!(system, "AIX" | "OS400"),
            apple: get("APPLE").is_some_and(crate::util::is_on),
            android: system == "Android",
        }
    }
}

impl Target {
    /// A bare target with no properties.  Commands create targets through
    /// `commands::target::create_target`, which also runs the cmTarget
    /// constructor initialization; this is for tests and synthetic targets.
    pub fn new(name: &str, kind: TargetKind, makefile: usize) -> Self {
        Target {
            name: name.to_owned(),
            kind,
            makefile,
            imported: false,
            imported_global: false,
            alias_of: None,
            props: Props::default(),
            provenance: BTreeMap::new(),
            backtrace: Vec::new(),
            policies: PolicyMap::default(),
            system_include_directories: BTreeSet::new(),
            dependencies: BTreeMap::new(),
            custom_commands: Vec::new(),
            file_sets: BTreeMap::new(),
            link_signatures: Vec::new(),
            have_install_rule: false,
            traced_utilities: Vec::new(),
            codegen_dependencies: BTreeSet::new(),
            unity_batched: BTreeSet::new(),
            find_package_stack: Vec::new(),
            synthetic: false,
            template_name: None,
            generator_provided: false,
            platform: Platform::default(),
        }
    }

    fn is_on(&self, property: &str) -> bool {
        crate::util::is_on_opt(self.props.get(property))
    }

    /// `cmTarget::IsExecutableWithExports`.
    pub fn is_executable_with_exports(&self) -> bool {
        self.kind == TargetKind::Executable && self.is_on("ENABLE_EXPORTS")
    }

    /// `cmTarget::IsSharedLibraryWithExports`.
    pub fn is_shared_library_with_exports(&self) -> bool {
        self.kind == TargetKind::SharedLibrary && self.is_on("ENABLE_EXPORTS")
    }

    /// `cmTarget::IsFrameworkOnApple`.
    pub fn is_framework_on_apple(&self) -> bool {
        self.platform.apple
            && matches!(
                self.kind,
                TargetKind::SharedLibrary | TargetKind::StaticLibrary
            )
            && self.is_on("FRAMEWORK")
    }

    /// `cmTarget::IsAppBundleOnApple`.
    pub fn is_app_bundle_on_apple(&self) -> bool {
        self.platform.apple && self.kind == TargetKind::Executable && self.is_on("MACOSX_BUNDLE")
    }

    /// `cmGeneratorTarget::IsCFBundleOnApple`.
    pub fn is_cf_bundle_on_apple(&self) -> bool {
        self.platform.apple && self.kind == TargetKind::ModuleLibrary && self.is_on("BUNDLE")
    }

    /// `cmGeneratorTarget::IsXCTestOnApple`.
    pub fn is_xctest_on_apple(&self) -> bool {
        self.is_cf_bundle_on_apple() && self.is_on("XCTEST")
    }

    /// `cmGeneratorTarget::IsBundleOnApple`.
    pub fn is_bundle_on_apple(&self) -> bool {
        self.is_framework_on_apple()
            || self.is_app_bundle_on_apple()
            || self.is_cf_bundle_on_apple()
    }

    /// `cmTarget::IsAndroidGuiExecutable`.
    pub fn is_android_gui_executable(&self) -> bool {
        self.platform.android && self.kind == TargetKind::Executable && self.is_on("ANDROID_GUI")
    }

    /// `cmTarget::IsArchivedAIXSharedLibrary`.
    pub fn is_archived_aix_shared_library(&self) -> bool {
        if self.kind != TargetKind::SharedLibrary || !self.platform.aix {
            return false;
        }
        if let Some(value) = self
            .props
            .get("AIX_SHARED_LIBRARY_ARCHIVE")
            .filter(|value| !value.is_empty())
        {
            return crate::util::is_on(value);
        }
        !self.imported && self.policies.get(182) == Some(crate::policy::PolicyStatus::New)
    }
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
    /// `cmCustomCommand::SetEscapeAllowMakeVars`, set when the command is
    /// attached at generate time.
    pub escape_allow_make_vars: bool,
    /// `cmCustomCommand::SetTarget`: the owning target's name for TARGET
    /// (PRE_BUILD/PRE_LINK/POST_BUILD) commands.
    pub target: Option<String>,
    #[allow(dead_code, reason = "custom command generation metadata")]
    pub backtrace: Vec<Frame>,
    #[allow(dead_code, reason = "custom command generation metadata")]
    pub policies: PolicyMap,
}

/// A custom-command action deferred to generate time
/// (`cmMakefile::AddGeneratorAction`).
#[derive(Debug)]
pub enum GeneratorAction {
    /// add_custom_command(OUTPUT): `detail::AddCustomCommandToOutput`.
    Output {
        command: CustomCommand,
        replace: bool,
    },
    /// add_custom_command(OUTPUT ... APPEND):
    /// `detail::AppendCustomCommandToOutput` (outputs[0] is the output).
    Append(CustomCommand),
    /// add_custom_command(TARGET): `detail::AddCustomCommandToTarget`.
    Target {
        target: String,
        command: CustomCommand,
    },
    /// add_custom_target with commands or depends: `detail::AddUtilityCommand`.
    Utility {
        target: String,
        command: CustomCommand,
    },
    /// The target check of fltk_wrap_ui.
    FltkTarget {
        makefile: usize,
        target: String,
        backtrace: Vec<Frame>,
    },
}

impl GeneratorAction {
    pub fn makefile(&self) -> usize {
        match self {
            Self::Output { command, .. }
            | Self::Append(command)
            | Self::Target { command, .. }
            | Self::Utility { command, .. } => command.makefile,
            Self::FltkTarget { makefile, .. } => *makefile,
        }
    }
}

/// `cmLocalGenerator::SourceEntry`: what produces an output path.
#[derive(Clone, Debug, Default)]
pub struct OutputSource {
    /// Index into `ext.sources[dir]` of the source carrying the command.
    pub source: Option<usize>,
    pub source_is_byproduct: bool,
    /// Key of a target whose utility or build-event command produces it.
    pub target: Option<String>,
}

#[derive(Debug)]
pub struct SourceFile {
    #[allow(
        dead_code,
        reason = "source file directory scope for property integration"
    )]
    pub makefile: usize,
    pub props: Props,
    /// `cmSourceFile`'s backtraced property lists (INCLUDE_DIRECTORIES,
    /// COMPILE_OPTIONS, COMPILE_DEFINITIONS): each write's value plus the
    /// makefile backtrace at write time.
    pub provenance: BTreeMap<String, Vec<PropertyEntry>>,
    pub location: SourceFileLocation,
    /// `cmSourceFile::GetDepends`.
    pub depends: Vec<String>,
    pub generated: bool,
    pub custom_command: Option<CustomCommand>,
}

#[derive(Clone, Debug)]
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
    /// Creation order within the directory (`cmMakefile::SourceGroups` is a
    /// vector appended to as groups are created).
    pub seq: usize,
}

/// `cmMakefile::GetOrCreateSourceGroup` on a directory's group map: an
/// existing group, or a new one recorded after every earlier group.
pub fn source_group_entry(
    groups: &mut BTreeMap<String, SourceGroup>,
    name: String,
) -> &mut SourceGroup {
    let seq = groups.len();
    groups.entry(name).or_insert_with(|| SourceGroup {
        files: BTreeSet::new(),
        regex: None,
        seq,
    })
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

/// A `file(GENERATE)` request (cmGeneratorExpressionEvaluationFile).
#[derive(Debug, Clone)]
pub struct EvaluationFile {
    pub dir: usize,
    pub input: String,
    pub input_is_content: bool,
    pub output: String,
    pub condition: String,
    pub target: String,
    pub newline: String,
    pub permissions: u32,
    pub backtrace: Vec<Frame>,
    /// `cmPolicies::PolicyStatus` of CMP0070 at the file(GENERATE) call.
    pub cmp0070: crate::policy::PolicyStatus,
}

/// Extension state for command implementations.
#[derive(Debug, Default)]
pub struct Ext {
    pub(crate) ctest_discovery: Option<crate::ctest::testfile::Discovery>,
    pub(crate) ctest_script: Option<crate::ctest::script::State>,
    /// `cmGlobalGenerator::ExtraGenerator`: the selected IDE extra
    /// generator name (`CodeBlocks`, `Kate`, ...), if any.
    pub extra_generator: Option<String>,
    pub install: crate::install_model::InstallState,
    pub targets: HashMap<String, Target>,
    /// Target names in creation order.
    pub target_order: Vec<String>,
    /// Alias name -> real target name.
    pub aliases: HashMap<String, String>,
    pub tests: Vec<Test>,
    pub execution_files: Vec<String>,
    /// Where the current directory's entries begin in `execution_files`
    /// (a directory snapshot has no call-stack parent).
    pub execution_file_bases: Vec<usize>,
    pub testing_enabled: BTreeSet<usize>,
    /// `cmGeneratorTarget::InfoByConfig::SyntheticDeps`: by target key and
    /// configuration, the synthetic BMI-only C++ module targets of each
    /// imported target in the link implementation.
    pub synthetic_deps: BTreeMap<(String, String), BTreeMap<String, Vec<String>>>,
    pub sources: BTreeMap<usize, Vec<SourceFile>>,
    pub generated_sources: BTreeSet<String>,
    /// Where `target_order` continues with targets the generator created
    /// after `CreateGenerationObjects`, behind the global targets.
    pub generated_targets_start: Option<usize>,
    pub source_groups: BTreeMap<usize, BTreeMap<String, SourceGroup>>,
    pub directory_provenance: BTreeMap<usize, BTreeMap<String, Vec<PropertyEntry>>>,
    pub directory_system_includes: BTreeMap<usize, BTreeSet<String>>,
    pub directory_define_flags: BTreeMap<usize, String>,
    pub pending_subdirs: BTreeMap<usize, Vec<PendingSubdirectory>>,
    /// `cmMakefile::GeneratorActions`, in configure order across all
    /// directories.  Consumed by `custom_actions::run_generator_actions`.
    pub generator_actions: Vec<GeneratorAction>,
    pub generator_actions_invoked: bool,
    /// `cmLocalGenerator::OutputToSource`, per directory, filled at generate
    /// time by `run_generator_actions`.
    pub output_to_source: BTreeMap<usize, HashMap<String, OutputSource>>,
    pub scoped_targets: HashMap<(usize, String), String>,
    /// Keys of directory-scoped targets by target name, in registration
    /// order (index for global lookups of promoted imported targets).
    pub scoped_keys_by_name: HashMap<String, Vec<String>>,
    /// `ext.sources` indices by (directory, name without a known source
    /// extension) for cmSourceFileLocation lookups.
    pub source_name_index: HashMap<(usize, String), Vec<usize>>,
    pub scoped_aliases: HashMap<(usize, String), String>,
    /// Creation backtraces for alias targets, `(directory, alias name)`;
    /// upstream keeps a `cmTarget` per alias in the directory's target
    /// list, which `cmGlobalGenerator::FindTarget` (and hence the
    /// reserved-name checks) can see.
    pub alias_backtraces: Vec<(usize, String, Vec<crate::messages::Frame>)>,
    pub target_property_initializers: BTreeMap<String, String>,
    pub defer_current_id: Option<String>,
    pub dependency_provider: Option<(String, Vec<String>)>,
    pub find_package: HashMap<usize, crate::commands::find::package::PackageState>,
    /// `cmState::PackageStateMap` for CPS packages: root `*.cps` file ->
    /// names of the CPS files (base plus appendices) already imported.
    pub cps_imported: HashMap<String, BTreeSet<String>>,
    pub next_defer_id: usize,
    pub experimental_warnings: std::cell::RefCell<std::collections::HashSet<(String, String)>>,
    pub in_top_level_includes: bool,
    pub message_checks: Vec<String>,

    pub property_definitions: crate::properties::Definitions,
    pub install_properties: HashMap<String, Props>,
    pub directory_definitions: HashMap<usize, BTreeMap<String, String>>,
    pub variable_watches: HashMap<String, Vec<String>>,
    pub active_variable_watches: std::collections::HashSet<(String, usize)>,
    pub host_information: Option<crate::commands::sysinfo::HostInformation>,
    pub distribution: Option<BTreeMap<String, String>>,
    pub file_api_queries: BTreeMap<String, (u32, u32)>,
    /// file(GENERATE) requests, evaluated at generate time.
    pub evaluation_files: Vec<EvaluationFile>,
    /// `cmGlobalGenerator::FilenameTargetDepends`: resolved file(GENERATE)
    /// output paths -> names of targets the output expression depends on
    /// (its source-sensitive targets). Filled during
    /// `CreateFileGenerateOutputs` and read by `TraceDependencies` to
    /// reject dependency loops.
    pub filename_target_depends: BTreeMap<String, BTreeSet<String>>,
    /// `cmSourceFile::FindFullPathFailed`: full path resolution failures
    /// are reported only once per source across the whole generation.
    pub reported_missing_sources: std::cell::RefCell<BTreeSet<String>>,
    /// `cmSourceFile::FullPath` caching: the CMP0115 extension warning is
    /// emitted only on the first successful resolution of a source.
    pub reported_source_warnings: std::cell::RefCell<BTreeSet<String>>,
    /// `cmGeneratorTarget::KindedSourcesMap`: (target,config) kinded-source
    /// computations that already ran. Diagnostics they raise are reported
    /// only on the first evaluation of the generation.
    pub computed_sources: std::cell::RefCell<BTreeSet<String>>,
    /// Free-form registry for command modules that need global state.
    pub misc: HashMap<String, String>,
    pub configure_dependencies: std::collections::BTreeSet<String>,
    pub configure_outputs: std::collections::BTreeSet<String>,
    pub glob_dependencies: Vec<crate::commands::file::GlobDependency>,
    /// `cmGlobVerificationManager` VerifyScript / VerifyStamp once saved.
    pub glob_verify: Option<(String, String)>,
    pub file_locks: crate::commands::file::LockPool,
    /// `cmMakefile::GetOrCreateConfigureLog` state (created lazily).
    pub configure_log: std::cell::RefCell<Option<crate::configure_log::ConfigureLog>>,
    /// `cmGlobalNinjaMultiGenerator` state, filled by `generator::generate`
    /// before `GenCtx::new`: whether the generator is multi-config, the
    /// `CMAKE_CONFIGURATION_TYPES` list, the default file configuration
    /// (`CMAKE_DEFAULT_BUILD_TYPE` or the first config), the
    /// `CMAKE_CROSS_CONFIGS` and `CMAKE_DEFAULT_CONFIGS` sets.
    pub multi_config: bool,
    /// No progress callback is installed (`cmake --system-information`):
    /// status lines are not printed.
    pub no_progress: bool,
    pub gen_configs: Vec<String>,
    pub gen_default_file_config: String,
    pub gen_cross_configs: BTreeSet<String>,
    pub gen_default_configs: BTreeSet<String>,
    /// `cmGlobalNinjaGenerator::PerConfigUtilityTargets`: utility targets
    /// whose outputs are per-config (created by `CreateUtilityOutput` in
    /// `cmLocalNinjaGenerator`).
    pub per_config_utility_targets: BTreeSet<String>,
    /// `cmGlobalGenerator::PerConfigModuleDbs`: configuration -> language ->
    /// build-database paths, registered as each generator target is created
    /// (`cmGeneratorTarget`'s constructor).
    pub per_config_module_dbs: BTreeMap<String, BTreeMap<String, Vec<String>>>,
    /// `cmGlobalGenerator::PerLanguageModuleDbs`: language -> paths.
    pub per_language_module_dbs: BTreeMap<String, Vec<String>>,
    /// `cmMakefile::GeneratorActions` recorded with
    /// `GeneratorActionWhen::AfterGeneratorTargets`: the
    /// `cmake_build_database*` merge commands resolve their input lists
    /// after synthetic targets are discovered.
    pub build_database_merges: Vec<BuildDatabaseMerge>,
}

/// Input selector for a `cmake_build_database*` merge command
/// (`ModuleCompilationDatabaseCommandAction`'s input closure).
#[derive(Debug)]
pub enum BuildDatabaseInputs {
    /// `PerConfigModuleDbs[config][lang]`.
    PerConfig { config: String, lang: String },
    /// `PerLanguageModuleDbs[lang]`.
    PerLanguage { lang: String },
    /// A fixed list (the per-config aggregate outputs).
    List(Vec<String>),
}

#[derive(Debug)]
pub struct BuildDatabaseMerge {
    pub comment: String,
    pub output: String,
    pub inputs: BuildDatabaseInputs,
}

#[derive(Debug)]
pub struct PendingSubdirectory {
    /// The cmMakefile created when the directory was added.
    pub index: usize,
    pub source: String,
    pub binary: String,
    pub exclude_from_all: bool,
    pub backtrace: Vec<Frame>,
    /// Function/block scopes active at the `subdirs()` call (outermost first).
    pub inner_scopes: Vec<u64>,
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
        .scoped_keys_by_name
        .get(name)?
        .iter()
        .find(|key| {
            interp
                .ext
                .targets
                .get(*key)
                .is_some_and(|target| target.imported_global)
        })
        .cloned()
}

/// `cmMakefile::FindTargetToUse`, also returning the target's key in
/// `interp.ext.targets` (which differs from `Target::name` for
/// directory-scoped imported targets).
pub fn find_target_entry<'a>(interp: &'a Interp, name: &str) -> Option<(&'a str, &'a Target)> {
    let key = find_target_key(interp, name)?;
    interp
        .ext
        .targets
        .get_key_value(&key)
        .map(|(key, target)| (key.as_str(), target))
}

/// `cmMakefile::FindTargetToUse`.
pub fn find_target_to_use<'a>(interp: &'a Interp, name: &str) -> Option<&'a Target> {
    interp.ext.targets.get(&find_target_key(interp, name)?)
}

#[allow(unused_imports, reason = "shared API for property command integration")]
pub use crate::commands::target::{
    PropertyWrite, find_source, get_or_create_source, mark_generated, read_target_property,
    register_target, source_prop, target_property, write_target_property,
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
    let bytes = crate::bytes::encode(text);
    let mut highest: Option<usize> = None;
    for index in 0..10 {
        let value = found.groups[index]
            .map(|(start, end)| crate::bytes::decode(&bytes[start..end]))
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
    crate::commands::path::equal(lhs, rhs)
}
