//! `cmPackageInfoArguments`: shared option parsing, validation and metadata
//! inheritance for `export(PACKAGE_INFO)` and `install(PACKAGE_INFO)`.

use crate::interp::{CmdResult, Interp, err};

/// Options bound for a package-info export (`cmPackageInfoArguments`).
#[derive(Clone, Debug, Default)]
pub struct PackageInfoArguments {
    /// `PACKAGE_INFO` (the package name).
    pub package_name: String,
    /// `LOWER_CASE_FILE`.
    pub lower_case: bool,
    /// `APPENDIX`.
    pub appendix: String,
    /// `VERSION`.
    pub version: String,
    /// `COMPAT_VERSION`.
    pub compat_version: String,
    /// `VERSION_SCHEMA`.
    pub version_schema: String,
    /// `DEFAULT_TARGETS`.
    pub default_targets: Vec<String>,
    /// `DEFAULT_CONFIGURATIONS`.
    pub default_configs: Vec<String>,
    /// `DESCRIPTION`.
    pub description: String,
    /// `HOMEPAGE_URL`.
    pub website: String,
    /// `PROJECT` (effective project after `SetEffectiveProject`).
    pub project_name: String,
    /// `NO_PROJECT_METADATA`.
    pub no_project_defaults: bool,
}

/// `cmPackageInfoArguments::Bind`: the keywords accepted in package-info mode.
pub const SINGLE_KEYS: [&str; 8] = [
    "PACKAGE_INFO",
    "APPENDIX",
    "VERSION",
    "COMPAT_VERSION",
    "VERSION_SCHEMA",
    "DESCRIPTION",
    "HOMEPAGE_URL",
    "PROJECT",
];
pub const LIST_KEYS: [&str; 2] = ["DEFAULT_TARGETS", "DEFAULT_CONFIGURATIONS"];
pub const FLAG_KEYS: [&str; 2] = ["LOWER_CASE_FILE", "NO_PROJECT_METADATA"];

impl PackageInfoArguments {
    /// Copy bound values out of a parsed argument map.
    pub(crate) fn from_args(args: &crate::commands::install::Arguments<'_>) -> Self {
        Self {
            package_name: args.get("PACKAGE_INFO").to_owned(),
            lower_case: args.has("LOWER_CASE_FILE"),
            appendix: args.get("APPENDIX").to_owned(),
            version: args.get("VERSION").to_owned(),
            compat_version: args.get("COMPAT_VERSION").to_owned(),
            version_schema: args.get("VERSION_SCHEMA").to_owned(),
            default_targets: args.list("DEFAULT_TARGETS").iter().map(|s| s.to_owned()).collect(),
            default_configs: args
                .list("DEFAULT_CONFIGURATIONS")
                .iter()
                .map(|s| s.to_owned())
                .collect(),
            description: args.get("DESCRIPTION").to_owned(),
            website: args.get("HOMEPAGE_URL").to_owned(),
            project_name: args.get("PROJECT").to_owned(),
            no_project_defaults: args.has("NO_PROJECT_METADATA"),
        }
    }

    /// `cmPackageInfoArguments::Check`.
    pub fn check(&self, enable: bool) -> CmdResult {
        fn requires(req: &str, specified: bool, arg: &str) -> CmdResult {
            if specified { err(format!("{arg} requires {req}.")) } else { Ok(()) }
        }
        fn exclusive(arg1: &str, specified: bool, arg2: &str) -> CmdResult {
            if specified { err(format!("{arg1} and {arg2} are mutually exclusive.")) } else { Ok(()) }
        }
        if !enable {
            // Check if any options were given.
            requires("PACKAGE_INFO", self.lower_case, "LOWER_CASE_FILE")?;
            requires("PACKAGE_INFO", !self.appendix.is_empty(), "APPENDIX")?;
            requires("PACKAGE_INFO", !self.version.is_empty(), "VERSION")?;
            requires("PACKAGE_INFO", !self.description.is_empty(), "DESCRIPTION")?;
            requires("PACKAGE_INFO", !self.website.is_empty(), "HOMEPAGE_URL")?;
            requires("PACKAGE_INFO", !self.default_targets.is_empty(), "DEFAULT_TARGETS")?;
            requires("PACKAGE_INFO", !self.default_configs.is_empty(), "DEFAULT_CONFIGURATIONS")?;
            requires("PACKAGE_INFO", !self.project_name.is_empty(), "PROJECT")?;
            requires("PACKAGE_INFO", self.no_project_defaults, "NO_PROJECT_METADATA")?;
        }

        // Check for incompatible options.
        if !self.appendix.is_empty() {
            exclusive("APPENDIX", !self.version.is_empty(), "VERSION")?;
            exclusive("APPENDIX", !self.description.is_empty(), "DESCRIPTION")?;
            exclusive("APPENDIX", !self.website.is_empty(), "HOMEPAGE_URL")?;
            exclusive("APPENDIX", !self.default_targets.is_empty(), "DEFAULT_TARGETS")?;
            exclusive("APPENDIX", !self.default_configs.is_empty(), "DEFAULT_CONFIGURATIONS")?;
            exclusive("APPENDIX", !self.project_name.is_empty(), "PROJECT")?;
        }
        if self.no_project_defaults {
            exclusive("PROJECT", !self.project_name.is_empty(), "NO_PROJECT_METADATA")?;
        }

        // Check for options that require other options.
        if self.version.is_empty() {
            requires("VERSION", !self.compat_version.is_empty(), "COMPAT_VERSION")?;
            requires("VERSION", !self.version_schema.is_empty(), "VERSION_SCHEMA")?;
        }

        // Validate the package name.
        if !self.package_name.is_empty()
            && (!crate::genex::is_valid_target_name(&self.package_name)
                || self.package_name.contains(':'))
        {
            return err(format!(
                "PACKAGE_INFO given invalid package name \"{}\".",
                self.package_name
            ));
        }
        Ok(())
    }

    /// `cmPackageInfoArguments::SetMetadataFromProject`.
    pub fn set_metadata_from_project(&mut self, interp: &mut Interp) -> CmdResult {
        // Determine what project to use for inherited metadata.
        self.set_effective_project(interp)?;
        if self.project_name.is_empty() {
            // We are not inheriting from a project.
            return Ok(());
        }
        let project = self.project_name.clone();
        let map_value = |interp: &Interp, arg: &mut String, suffix: &str| -> bool {
            if let Some(value) = interp.get_def(&format!("{project}_{suffix}")) {
                *arg = value.to_owned();
                return true;
            }
            false
        };
        if self.version.is_empty() && map_value(interp, &mut self.version, "VERSION") {
            map_value(interp, &mut self.compat_version, "COMPAT_VERSION");
        }
        if self.description.is_empty() {
            map_value(interp, &mut self.description, "DESCRIPTION");
        }
        if self.website.is_empty() {
            map_value(interp, &mut self.website, "HOMEPAGE_URL");
        }
        Ok(())
    }

    /// `cmPackageInfoArguments::SetEffectiveProject`.
    fn set_effective_project(&mut self, interp: &mut Interp) -> CmdResult {
        if !self.appendix.is_empty() {
            // Appendices are not allowed to specify package metadata.
            return Ok(());
        }
        if self.no_project_defaults {
            // User requested that metadata not be inherited.
            return Ok(());
        }
        if !self.project_name.is_empty() {
            // User specified a project; make sure it exists.
            if !check_project_name(interp, &self.project_name) {
                return err(format!(
                    "PROJECT given invalid project name \"{}\".",
                    self.project_name
                ));
            }
        } else {
            // No project was specified; check if the package name is also a
            // project.
            let project = project_name(interp);
            if self.package_name == project {
                self.project_name = project;
            }
        }
        Ok(())
    }

    /// `cmPackageInfoArguments::GetNamespace`.
    pub fn namespace(&self) -> String {
        format!("{}::", self.package_name)
    }

    /// `cmPackageInfoArguments::GetPackageDirName`.
    pub fn package_dir_name(&self) -> String {
        if self.lower_case { self.package_name.to_lowercase() } else { self.package_name.clone() }
    }

    /// `cmPackageInfoArguments::GetPackageFileName`.
    pub fn package_file_name(&self) -> String {
        let dir = self.package_dir_name();
        if self.appendix.is_empty() {
            format!("{dir}.cps")
        } else {
            format!("{dir}-{}.cps", self.appendix)
        }
    }
}

/// `cmStateSnapshot::SetProjectName`: remember that `name` was used in a
/// `project()` call in the current directory.
pub(crate) fn record_project(interp: &mut Interp, name: &str) {
    let key = format!("project-names:{}", interp.mf_index());
    let entry = interp.ext.misc.entry(key).or_default();
    for existing in crate::util::expand_list(entry, false) {
        if existing == name {
            return;
        }
    }
    if entry.is_empty() {
        *entry = name.to_owned();
    } else {
        entry.push(';');
        entry.push_str(name);
    }
}

/// `cmStateSnapshot::CheckProjectName`.
pub(crate) fn check_project_name(interp: &Interp, name: &str) -> bool {
    interp
        .ext
        .misc
        .get(&format!("project-names:{}", interp.mf_index()))
        .is_some_and(|list| crate::util::expand_list(list, false).iter().any(|n| n == name))
}

/// `cmStateSnapshot::GetProjectName`.
pub(crate) fn project_name(interp: &Interp) -> String {
    interp.mf().props.get("PROJECT_NAME").unwrap_or_default().to_owned()
}


