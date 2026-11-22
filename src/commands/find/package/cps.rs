//! Common Package Specification (`*.cps`) support: a port of
//! cmPackageInfoReader.cxx plus the CPS paths of cmFindPackageCommand.cxx.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::rc::Rc;

use serde_json::Value;

use crate::cache::convert_to_unix_slashes;
use crate::interp::{CmdResult, Interp, err};
use crate::messages::{Frame, MessageType};
use crate::state::TargetKind;
use crate::util::{CompareOp, filename_name, filename_path, is_full_path, version_compare};

use super::*;

/// Activation UUID for `cmExperimental::Feature::ImportPackageInfo`.
const IMPORT_PACKAGE_INFO_UUID: &str = "e82e467b-f997-4464-8ace-b00808fff261";
const IMPORT_PACKAGE_INFO_VARIABLE: &str = "CMAKE_EXPERIMENTAL_FIND_CPS_PACKAGES";
const IMPORT_PACKAGE_INFO_DESCRIPTION: &str = "CMake's support for importing package information in the Common Package Specification format (via find_package) is experimental. It is meant only for experimentation and feedback to CMake developers.";

/// `cmExperimental::HasSupportEnabled(Feature::ImportPackageInfo)`.
pub(super) fn import_package_info_enabled(interp: &mut Interp) -> bool {
    let Some(value) = interp.read_definition(IMPORT_PACKAGE_INFO_VARIABLE) else {
        return false;
    };
    let enabled = value == IMPORT_PACKAGE_INFO_UUID;
    // `cmGlobalGenerator::ShouldWarnExperimental`: warn once per
    // feature-name/value pair.
    if interp
        .ext
        .experimental_warnings
        .insert(("ImportPackageInfo".to_owned(), value.clone()))
    {
        let warning = if enabled {
            IMPORT_PACKAGE_INFO_DESCRIPTION.to_owned()
        } else {
            format!(
                "{IMPORT_PACKAGE_INFO_VARIABLE} is set to incorrect value\n  {value}\nSee 'Help/dev/experimental.rst' in the source tree of this version of CMake for documentation of the experimental feature and the corresponding activation value.  This project's code may require changes to work with this CMake's version of the feature."
            )
        };
        interp.issue(MessageType::AuthorWarning, &warning);
    }
    enabled
}

/// The last extension of `config` is `.cps` (case-insensitive), like the
/// check in `cmFindPackageCommand::CheckVersion`.
pub(super) fn is_cps(config: &str) -> bool {
    config
        .rfind('.')
        .is_some_and(|index| config[index..].eq_ignore_ascii_case(".cps"))
}

/// The package-description kind implied by a config candidate name.
pub(super) fn config_desc(name: &str) -> Desc {
    if is_cps(name) { Desc::Cps } else { Desc::CMake }
}

/// `cmMakefile::FindImportedTarget`: an imported target visible here.
fn imported_target_exists(interp: &Interp, name: &str) -> bool {
    crate::state::find_target_to_use(interp, name).is_some_and(|target| target.imported)
}

/// Map of CPS language names to CMake language names (`MapLanguage`).
/// Case insensitivity is achieved by lowercasing the CPS value.
fn map_language(lang: &str, glob: bool) -> Option<&'static str> {
    if glob && lang == "*" {
        return Some("*");
    }
    Some(match lang.to_ascii_lowercase().as_str() {
        "c" => "C",
        "c++" | "cpp" | "cxx" => "CXX",
        "objc" => "OBJC",
        "objc++" | "objcpp" | "objcxx" => "OBJCXX",
        "swift" => "swift",
        "hip" => "HIP",
        "cuda" => "CUDA",
        "ispc" => "ISPC",
        "c#" | "csharp" => "CSharp",
        "fortran" => "Fortran",
        _ => return None,
    })
}

fn real_path(path: &str) -> String {
    fs::canonicalize(path)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_owned())
}

fn real_dir(path: &str) -> String {
    filename_path(&real_path(path))
}

fn read_json(path: &str) -> Option<Value> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// `CheckSchemaVersion`: `cps_version` must exist and be >= 0.13 and < 1.
fn check_schema_version(data: &Value) -> bool {
    let version = data["cps_version"].as_str().unwrap_or("");
    !version.is_empty()
        && version_compare(CompareOp::GreaterEqual, version, "0.13")
        && version_compare(CompareOp::Less, version, "1")
}

/// `DeterminePrefix`: `prefix` absolute override, else `cps_path`
/// expressed relative to `@prefix@`.
fn determine_prefix(filepath: &str, data: &Value) -> String {
    // First check if an absolute prefix was supplied.
    let prefix = convert_to_unix_slashes(data["prefix"].as_str().unwrap_or(""));
    if !prefix.is_empty() {
        if is_full_path(&prefix) && Path::new(&prefix).is_dir() {
            return prefix;
        }
        // The specified absolute prefix is not valid.
        return String::new();
    }

    // Get and validate prefix-relative path.
    let abs_path = filename_path(filepath);
    let rel_path = convert_to_unix_slashes(data["cps_path"].as_str().unwrap_or(""));
    if rel_path.len() < 8 || !rel_path.starts_with("@prefix@") {
        // The relative prefix is not valid.
        return String::new();
    }
    if rel_path.len() == 8 {
        // The relative path is exactly "@prefix@".
        return abs_path;
    }
    if rel_path.as_bytes()[8] != b'/' {
        // The relative prefix is not valid.
        return String::new();
    }
    let rel_path = &rel_path[8..];

    // Get directory portion of the absolute path.
    if abs_path.ends_with(rel_path) {
        return abs_path[..abs_path.len() - rel_path.len()].to_owned();
    }
    for real in [real_path, real_dir] {
        let tmp_path = real(&abs_path);
        if tmp_path != abs_path && tmp_path.ends_with(rel_path) {
            return tmp_path[..tmp_path.len() - rel_path.len()].to_owned();
        }
    }
    String::new()
}

/// `ReadList`: only string members of an array value are returned.
fn read_list(data: &Value, key: &str) -> Vec<String> {
    read_list_value(&data[key])
}

fn read_list_value(arr: &Value) -> Vec<String> {
    match arr {
        Value::Array(items) => items
            .iter()
            .filter_map(|item| item.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

/// `Json::Value::asString` for scalar values; objects and arrays convert
/// to an empty string (upstream treats that as a programming error).
fn json_string(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Bool(flag) => if *flag { "true" } else { "false" }.to_owned(),
        Value::Number(number) => number.to_string(),
        _ => String::new(),
    }
}

/// `NormalizeTargetName`: `:local` -> `pkg::local`, `pkg:comp` ->
/// `pkg::comp`, anything else unchanged.
fn normalize_target_name(name: &str, context: &str) -> String {
    if name.starts_with(':') {
        return format!("{context}:{name}");
    }
    if let Some(index) = name.find(':') {
        return format!("{}:{}", &name[..index], &name[index..]);
    }
    name.to_owned()
}

fn append_property(
    interp: &mut Interp,
    target: &str,
    property: &str,
    configuration: &str,
    value: &str,
) {
    let full = if configuration.is_empty() {
        format!("INTERFACE_{property}")
    } else {
        format!(
            "INTERFACE_{property}_{}",
            configuration.to_ascii_uppercase()
        )
    };
    if let Some(target) = interp.ext.targets.get_mut(target) {
        target.props.append(&full, value, false);
    }
}

/// `AppendLanguageProperties`: `data[key]` may be a list applied to all
/// languages or an object mapping CPS languages to lists.
#[allow(clippy::too_many_arguments)] // mirrors the upstream signature
fn append_language_properties(
    interp: &mut Interp,
    target: &str,
    target_name: &str,
    property: &str,
    configuration: &str,
    data: &Value,
    key: &str,
    transform: &dyn Fn(&str) -> String,
) {
    match &data[key] {
        Value::Array(_) => {
            for value in read_list_value(&data[key]) {
                append_property(interp, target, property, configuration, &transform(&value));
            }
        }
        Value::Object(by_language) => {
            for (original, values) in by_language {
                let Some(lang) = map_language(original, true) else {
                    interp.issue(
                        MessageType::Warning,
                        &format!(
                            "ignoring unknown language \"{original}\" in {key} for {target_name}"
                        ),
                    );
                    continue;
                };
                if lang == "*" {
                    for value in read_list_value(values) {
                        append_property(
                            interp,
                            target,
                            property,
                            configuration,
                            &transform(&value),
                        );
                    }
                } else {
                    for value in read_list_value(values) {
                        let value = format!("$<$<COMPILE_LANGUAGE:{lang}>:{}>", transform(&value));
                        append_property(interp, target, property, configuration, &value);
                    }
                }
            }
        }
        _ => {}
    }
}

/// `AddCompileFeature`: `cNN`/`cxxNN` standards, `gnu`, and `threads`.
fn add_compile_feature(interp: &mut Interp, target: &str, configuration: &str, value: &str) {
    // "^[Cc]([+][+])?([0-9][0-9])$"
    let bytes = value.as_bytes();
    let feature = match bytes {
        [b'c' | b'C', a, b] if a.is_ascii_digit() && b.is_ascii_digit() => {
            Some(format!("c_std_{}{}", *a as char, *b as char))
        }
        [b'c' | b'C', b'+', b'+', a, b] if a.is_ascii_digit() && b.is_ascii_digit() => {
            Some(format!("cxx_std_{}{}", *a as char, *b as char))
        }
        _ => None,
    };
    if let Some(feature) = feature {
        if configuration.is_empty() {
            append_property(interp, target, "COMPILE_FEATURES", "", &feature);
        } else {
            let feature = format!("$<$<CONFIG:{configuration}>:{feature}>");
            append_property(interp, target, "COMPILE_FEATURES", "", &feature);
        }
    } else if value.eq_ignore_ascii_case("gnu") {
        // Not implemented in CMake at this time
    } else if value.eq_ignore_ascii_case("threads") {
        append_property(
            interp,
            target,
            "LINK_LIBRARIES",
            configuration,
            "Threads::Threads",
        );
    }
}

/// `AddLinkFeature`.
fn add_link_feature(interp: &mut Interp, target: &str, configuration: &str, value: &str) {
    if value.eq_ignore_ascii_case("thread") {
        append_property(
            interp,
            target,
            "LINK_LIBRARIES",
            configuration,
            "Threads::Threads",
        );
    }
}

/// `BuildDefinition`.
fn build_definition(name: &str, value: &Value) -> String {
    if !value.is_null() {
        // isConvertibleTo(stringValue): numbers, booleans, strings.
        if !matches!(value, Value::Array(_) | Value::Object(_)) {
            return format!("{name}={}", json_string(value));
        }
    }
    name.to_owned()
}

/// `AddDefinitions`: `definitions` maps each definition name to the
/// values declared for individual CMake languages (`*` meaning all).
fn add_definitions(
    interp: &mut Interp,
    target: &str,
    configuration: &str,
    definitions: &BTreeMap<String, BTreeMap<String, Value>>,
) {
    for (name, by_language) in definitions {
        if let Some(value) = by_language.get("*") {
            let definition = build_definition(name, value);
            if by_language.len() == 1 {
                // Only the non-language-specific definition exists.
                append_property(
                    interp,
                    target,
                    "COMPILE_DEFINITIONS",
                    configuration,
                    &definition,
                );
                continue;
            }
            // Create a genex to apply this definition to all languages
            // except those that override it.
            let excluded: Vec<&String> = by_language
                .keys()
                .filter(|lang| lang.as_str() != "*")
                .collect();
            let excluded = excluded
                .iter()
                .map(|lang| lang.as_str())
                .collect::<Vec<_>>()
                .join(",");
            append_property(
                interp,
                target,
                "COMPILE_DEFINITIONS",
                configuration,
                &format!("$<$<NOT:$<COMPILE_LANGUAGE:{excluded}>>:{definition}>"),
            );
        }
        // Add language-specific definitions.
        for (lang, value) in by_language {
            if lang != "*" {
                let definition = format!(
                    "$<$<COMPILE_LANGUAGE:{lang}>:{}>",
                    build_definition(name, value)
                );
                append_property(
                    interp,
                    target,
                    "COMPILE_DEFINITIONS",
                    configuration,
                    &definition,
                );
            }
        }
    }
}

/// `cmPackageInfoReader::Pep440Version` produced by `ParseSimpleVersion`.
struct Pep440 {
    components: Vec<u32>,
}

/// `ParseSimpleVersion`: a numeric tuple followed by an optional local
/// label introduced by `+` or `-`.
fn parse_simple_version(version: &str) -> Option<Pep440> {
    if version.is_empty() {
        return None;
    }
    let mut components = Vec::new();
    let mut remnant = version;
    loop {
        let index = remnant.find(['.', '+', '-']);
        let (part, rest) = match index {
            Some(0) => return None,
            Some(index) => remnant.split_at(index),
            None => (remnant, ""),
        };
        // std::stoul accepts a leading '+'; a '-' would overflow below.
        if part.is_empty()
            || !part[1..].bytes().all(|byte| byte.is_ascii_digit())
            || !(part.as_bytes()[0].is_ascii_digit() || part.as_bytes()[0] == b'+')
        {
            return None;
        }
        let value: u64 = part.parse().ok()?;
        if value > u64::from(u32::MAX) {
            return None;
        }
        components.push(value as u32);
        if index.is_none() {
            return Some(Pep440 { components });
        }
        let separator = rest.as_bytes()[0];
        remnant = &rest[1..];
        if separator == b'+' || separator == b'-' {
            // If we hit the local label, we're done.
            return Some(Pep440 { components });
        }
        if remnant.is_empty() {
            // A trailing part separator is not allowed.
            return None;
        }
    }
}

/// A requirement declared in a package's `requires` object
/// (`cmPackageRequirement`).
pub(super) struct PackageRequirement {
    pub name: String,
    pub version: String,
    pub components: Vec<String>,
    #[allow(dead_code, reason = "upstream does not consume hints yet")]
    pub hints: Vec<String>,
}

/// `cmPackageInfoReader`: a parsed CPS file plus the resolved prefix.
pub(super) struct Reader {
    data: Value,
    prefix: String,
    path: String,
    default_configurations: Vec<String>,
}

impl Reader {
    /// `cmPackageInfoReader::Read`.  A `parent` reader marks a
    /// supplemental file, which inherits the prefix and default
    /// configurations and skips the `cps_version` check.
    fn read(path: &str, parent: Option<&Reader>) -> Option<Reader> {
        // Read file and perform some basic validation:
        //   - the input is valid JSON
        //   - the input is a JSON object
        //   - the input has a "cps_version" that we (in theory) know
        //     how to parse
        let data = read_json(path)?;
        if !data.is_object() || (parent.is_none() && !check_schema_version(&data)) {
            return None;
        }

        //   - the input has a "name" attribute that is a non-empty string
        let name = &data["name"];
        if !name.is_string() || name.as_str().is_none_or(str::is_empty) {
            return None;
        }

        //   - the input has a "components" attribute that is a JSON object
        if !data["components"].is_object() {
            return None;
        }

        let prefix = match parent {
            Some(parent) => parent.prefix.clone(),
            None => determine_prefix(path, &data),
        };
        if prefix.is_empty() {
            return None;
        }

        let default_configurations = match parent {
            Some(parent) => parent.default_configurations.clone(),
            None => read_list(&data, "configurations")
                .iter()
                .map(|config| config.to_ascii_uppercase())
                .collect(),
        };

        Some(Reader {
            data,
            prefix,
            path: path.to_owned(),
            default_configurations,
        })
    }

    fn name(&self) -> &str {
        self.data["name"].as_str().unwrap_or("")
    }

    fn version(&self) -> Option<&str> {
        self.data["version"].as_str()
    }

    fn compat_version(&self) -> Option<&str> {
        self.data["compat_version"].as_str()
    }

    /// `cmPackageInfoReader::ParseVersion`: the `simple` schema (the
    /// default) parses to a numeric tuple; other schemas return `None`.
    fn parse_version(&self, version: Option<&str>) -> Option<Pep440> {
        let version = version?;
        let schema = &self.data["version_schema"];
        if schema.is_null()
            || schema
                .as_str()
                .is_some_and(|schema| schema.eq_ignore_ascii_case("simple"))
        {
            return parse_simple_version(version);
        }
        None
    }

    /// `cmPackageInfoReader::GetRequirements`.
    fn requirements(&self) -> Vec<PackageRequirement> {
        let mut requirements = Vec::new();
        if let Value::Object(required) = &self.data["requires"] {
            for (name, value) in required {
                requirements.push(PackageRequirement {
                    name: name.clone(),
                    version: json_string(&value["version"]),
                    components: read_list(value, "components"),
                    hints: read_list(value, "hints"),
                });
            }
        }
        requirements
    }

    /// `cmPackageInfoReader::GetComponentNames`.
    fn component_names(&self) -> Vec<String> {
        self.data["components"]
            .as_object()
            .map(|components| components.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// `cmPackageInfoReader::ResolvePath`.
    fn resolve_path(&self, path: &str) -> String {
        let path = convert_to_unix_slashes(path);
        if let Some(rest) = path.strip_prefix("@prefix@") {
            return format!("{}{rest}", self.prefix);
        }
        if !is_full_path(&path) {
            return format!("{}/{path}", filename_path(&self.path));
        }
        path
    }

    /// `SetImportProperty`: `IMPORTED_<property>[_<CONFIG>]` resolved
    /// against the package prefix.
    fn set_import_property(
        &self,
        interp: &mut Interp,
        target: &str,
        property: &str,
        configuration: &str,
        value: &Value,
    ) {
        if !value.is_null() {
            let full = if configuration.is_empty() {
                format!("IMPORTED_{property}")
            } else {
                format!("IMPORTED_{property}_{}", configuration.to_ascii_uppercase())
            };
            let resolved = self.resolve_path(&json_string(value));
            if let Some(target) = interp.ext.targets.get_mut(target) {
                target.props.set(&full, resolved);
            }
        }
    }

    /// `SetTargetProperties`.
    fn set_target_properties(
        &self,
        interp: &mut Interp,
        target: &str,
        target_name: &str,
        data: &Value,
        package: &str,
        configuration: &str,
    ) {
        // Add configuration (if applicable).
        if !configuration.is_empty() {
            if let Some(target) = interp.ext.targets.get_mut(target) {
                target.props.append(
                    "IMPORTED_CONFIGURATIONS",
                    &configuration.to_ascii_uppercase(),
                    false,
                );
            }
        }

        // Add compile and link features.
        for feature in read_list(data, "compile_features") {
            add_compile_feature(interp, target, configuration, &feature);
        }
        for feature in read_list(data, "link_features") {
            add_link_feature(interp, target, configuration, &feature);
        }

        // Add compile definitions.
        let mut definitions: BTreeMap<String, BTreeMap<String, Value>> = BTreeMap::new();
        if let Value::Object(by_language) = &data["definitions"] {
            for (original, entries) in by_language {
                let Some(lang) = map_language(original, true) else {
                    interp.issue(
                        MessageType::Warning,
                        &format!(
                            "ignoring unknown language \"{original}\" in definitions for {target_name}"
                        ),
                    );
                    continue;
                };
                if let Value::Object(entries) = entries {
                    for (name, value) in entries {
                        definitions
                            .entry(name.clone())
                            .or_default()
                            .entry(lang.to_owned())
                            .or_insert_with(|| value.clone());
                    }
                }
            }
        }
        add_definitions(interp, target, configuration, &definitions);

        // Add include directories.
        let reader = self;
        append_language_properties(
            interp,
            target,
            target_name,
            "INCLUDE_DIRECTORIES",
            configuration,
            data,
            "includes",
            &|path| reader.resolve_path(path),
        );

        // Add link name/location(s).
        self.set_import_property(interp, target, "LOCATION", configuration, &data["location"]);
        self.set_import_property(
            interp,
            target,
            "IMPLIB",
            configuration,
            &data["link_location"],
        );
        self.set_import_property(interp, target, "SONAME", configuration, &data["link_name"]);

        // Add link languages.
        for original in read_list(data, "link_languages") {
            if let Some(lang) = map_language(&original, false) {
                append_property(interp, target, "LINK_LANGUAGES", configuration, lang);
            }
        }

        // Add transitive dependencies.
        for dep in read_list(data, "requires") {
            append_property(
                interp,
                target,
                "LINK_LIBRARIES",
                configuration,
                &normalize_target_name(&dep, package),
            );
        }
        for dep in read_list(data, "link_requires") {
            let lib = format!("$<LINK_ONLY:{}>", normalize_target_name(&dep, package));
            append_property(interp, target, "LINK_LIBRARIES", configuration, &lib);
        }

        // Add other information.
        if configuration.is_empty() {
            let license = &data["license"];
            if !license.is_null()
                && let Some(target) = interp.ext.targets.get_mut(target)
            {
                target.props.set("SPDX_LICENSE", json_string(license));
            }
        }
    }

    /// `AddLibraryComponent`: create the imported target and apply the
    /// component's default and per-configuration data.
    fn add_library_component(
        &self,
        interp: &mut Interp,
        kind: TargetKind,
        name: &str,
        data: &Value,
        package: &str,
    ) -> String {
        let key = crate::commands::target::create_target(interp, name, kind, true, false);
        // Set default configurations.
        if !self.default_configurations.is_empty()
            && let Some(target) = interp.ext.targets.get_mut(&key)
        {
            target.props.set(
                "IMPORTED_CONFIGURATIONS",
                self.default_configurations.join(";"),
            );
        }
        // Set target properties.
        self.set_target_properties(interp, &key, name, data, package, "");
        if let Value::Object(configurations) = &data["configurations"] {
            for (configuration, config_data) in configurations {
                self.set_target_properties(interp, &key, name, config_data, package, configuration);
            }
        }
        key
    }

    /// `cmPackageInfoReader::ImportTargets`.
    fn import_targets(
        &self,
        interp: &mut Interp,
        component_targets: &mut BTreeMap<String, String>,
    ) -> CmdResult {
        let package = self.name().to_owned();

        // Read components.
        let Value::Object(components) = &self.data["components"] else {
            return Ok(());
        };
        for (name, data) in components {
            let type_name = json_string(&data["type"]).to_ascii_lowercase();
            // Get and validate full target name.
            let full_name = format!("{package}::{name}");
            crate::commands::target::enforce_unique_name(interp, &full_name, false)?;

            let kind = match type_name.as_str() {
                "symbolic" => None, // TODO
                "dylib" => Some(TargetKind::SharedLibrary),
                "module" => Some(TargetKind::ModuleLibrary),
                "archive" => Some(TargetKind::StaticLibrary),
                "interface" => Some(TargetKind::InterfaceLibrary),
                _ => {
                    interp.issue(
                        MessageType::Warning,
                        &format!("component \"{full_name}\" has unknown type \"{type_name}\" and was not imported"),
                    );
                    None
                }
            };
            if let Some(kind) = kind {
                let key = self.add_library_component(interp, kind, &full_name, data, &package);
                component_targets.insert(name.clone(), key);
            }
        }

        // Read default components.
        let default_components = read_list(&self.data, "default_components");
        if !default_components.is_empty() {
            crate::commands::target::enforce_unique_name(interp, &package, false)?;
            let key = crate::commands::target::create_target(
                interp,
                &package,
                TargetKind::InterfaceLibrary,
                true,
                false,
            );
            for name in &default_components {
                let full_name = format!("{package}::{name}");
                append_property(interp, &key, "LINK_LIBRARIES", "", &full_name);
            }
        }

        Ok(())
    }

    /// `cmPackageInfoReader::ImportTargetConfigurations` for a
    /// configuration-specific supplemental (`<base>@<config>.cps`) file.
    fn import_target_configurations(
        &self,
        interp: &mut Interp,
        component_targets: &mut BTreeMap<String, String>,
    ) -> CmdResult {
        let configuration = json_string(&self.data["configuration"]);
        if configuration.is_empty() {
            interp.issue(
                MessageType::Warning,
                &format!(
                    "supplemental file {} does not specify a configuration",
                    self.path
                ),
            );
            return Ok(());
        }

        let package = self.name().to_owned();
        let Value::Object(components) = &self.data["components"] else {
            return Ok(());
        };
        for (name, data) in components {
            let Some(target) = component_targets.get(name).cloned() else {
                return err(format!("component {name} was not found"));
            };
            self.set_target_properties(interp, &target, name, data, &package, &configuration);
        }
        Ok(())
    }
}

/// The reader and appendices accepted by `check_version` for a `*.cps`
/// candidate (`CpsReader` plus `CpsAppendices`).
pub(super) struct CpsState {
    reader: Reader,
    /// Appendix path -> (reader, component names it provides).
    appendices: BTreeMap<String, (Reader, Vec<String>)>,
}

/// `cmFindPackageCommand::FindAppendices`: `<base>[-:]*.cps` files next
/// to `base`, excluding `@` (configuration-specific) files.
fn find_appendices(
    pkg: &Package,
    base: &str,
    base_reader: &Reader,
) -> BTreeMap<String, (Reader, Vec<String>)> {
    let mut appendices = BTreeMap::new();
    let dir = filename_path(base);
    let name = filename_name(base);
    let stem = &name[..name.rfind('.').unwrap_or(name.len())];
    let mut entries: Vec<String> = fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    entries.sort();
    for entry in entries {
        let Some(rest) = entry.strip_prefix(stem) else {
            continue;
        };
        if !(rest.starts_with('-') || rest.starts_with(':'))
            || entry.contains('@')
            || !rest[1..].to_ascii_lowercase().ends_with(".cps")
        {
            continue;
        }
        let extra = format!("{dir}/{entry}");
        if let Some(reader) = Reader::read(&extra, Some(base_reader))
            && reader.name() == pkg.name
        {
            let components = reader.component_names();
            appendices.insert(extra, (reader, components));
        }
    }
    appendices
}

/// `<base>@*.cps` files next to `base` (configuration-specific
/// supplemental files).
fn find_supplementals(base: &str) -> Vec<String> {
    let dir = filename_path(base);
    let name = filename_name(base);
    let stem = &name[..name.rfind('.').unwrap_or(name.len())];
    let mut entries: Vec<String> = fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    entries.sort();
    entries
        .into_iter()
        .filter(|entry| {
            entry.strip_prefix(stem).is_some_and(|rest| {
                rest.starts_with('@') && rest[1..].to_ascii_lowercase().ends_with(".cps")
            })
        })
        .map(|entry| format!("{dir}/{entry}"))
        .collect()
}

/// `cmFindPackageCommand::CheckVersion` for `*.cps` candidates: parse
/// the file, compare `version`/`compat_version`/`version_schema`, and
/// collect appendices providing requested components.
pub(super) fn check_version(pkg: &mut Package, config: &str) -> bool {
    let mut result = false;
    let mut version = "unknown".to_owned();

    if let Some(reader) = Reader::read(config, None)
        && reader.name() == pkg.name
    {
        // Read version information.
        let cps_version = reader.version().map(str::to_owned);
        let parsed_version = reader.parse_version(reader.version());
        let has_version = cps_version.is_some();

        // Test for version compatibility.
        result = pkg.version.min.is_empty();
        if let Some(found) = cps_version {
            version = found;
            if !pkg.version.min.is_empty() {
                if parsed_version.is_none() {
                    // If we don't understand the version, compare the exact
                    // versions using full string comparison.  This is the
                    // correct behavior for the "custom" schema, and the best
                    // we can do otherwise.
                    result = pkg.version.min == version;
                } else if pkg.version.exact {
                    // If EXACT is specified, the version must be exactly the
                    // requested version.
                    result = version_compare(CompareOp::Equal, &pkg.version.min, &version);
                } else if reader.parse_version(reader.compat_version()).is_some() {
                    // If yes, the initial result is whether the requested
                    // version is between the actual version and the compat
                    // version, inclusive.
                    let compat = reader.compat_version().unwrap_or_default().to_owned();
                    result = version_compare(CompareOp::GreaterEqual, &version, &pkg.version.min)
                        && version_compare(CompareOp::GreaterEqual, &pkg.version.min, &compat);
                    if result && !pkg.version.max.is_empty() {
                        // We must also check that the version is less than
                        // the version limit.
                        result = if pkg.version.max_excluded {
                            version_compare(CompareOp::Greater, &pkg.version.max, &version)
                        } else {
                            version_compare(CompareOp::GreaterEqual, &pkg.version.max, &version)
                        };
                    }
                } else {
                    // If no, compat_version is assumed to be exactly the
                    // actual version, so the result is whether the requested
                    // version is exactly the actual version, and we can
                    // ignore the version limit.
                    result = version_compare(CompareOp::Equal, &pkg.version.min, &version);
                }
            }
        }

        if result {
            // Locate appendices.
            let appendices = find_appendices(pkg, config, &reader);

            // Collect available components.
            let mut all_components: BTreeSet<String> =
                reader.component_names().into_iter().collect();
            for (_, components) in appendices.values() {
                all_components.extend(components.iter().cloned());
            }

            // Verify that all required components are available.
            let required: BTreeSet<String> =
                pkg.required.union(&pkg.required_targets).cloned().collect();
            if required.difference(&all_components).next().is_some() {
                result = false;
            }

            if result && has_version {
                pkg.selected_version.clone_from(&version);
                pkg.found_version_parts = parsed_version.map(|parsed| parsed.components);
            }
            pkg.cps = Some(CpsState { reader, appendices });
            pkg.required = required;
        }
    }

    pkg.considered.push((config.to_owned(), version));
    result
}

/// `cmFindPackageCommand::ReadPackage`: resolve dependencies and import
/// targets for an accepted `*.cps` file and its appendices.
pub(super) fn read_package(
    pkg: &mut Package,
    interp: &mut Interp,
    file: String,
    state: CpsState,
) -> CmdResult {
    // Resolve any transitive dependencies for the root file.
    pkg.find_package_dependencies(interp, &file, &state.reader, pkg.requirement)?;

    let has_components_requested = !pkg.required.is_empty() || !pkg.optional.is_empty();

    // `cmMakefile::CallRAII` scope for the root file.
    push_file_frame(interp, &file);

    // Loop over appendices.
    let mut appendices = Vec::new();
    for (path, (reader, components)) in state.appendices {
        let mut required = Requirement::Default;
        let mut important = false;

        // Check if this appendix provides any requested components.
        if has_components_requested {
            if components.iter().any(|c| pkg.required.contains(c)) {
                important = true;
                required = pkg.requirement;
            } else if !components.iter().any(|c| pkg.optional.contains(c)) {
                // This appendix doesn't provide any requested components;
                // don't import it.
                continue;
            }
        }

        // Resolve any transitive dependencies for the appendix.
        match pkg.find_package_dependencies(interp, &path, &reader, required) {
            Ok(()) => appendices.push((path, reader)),
            Err(error) if important => {
                // Some dependencies are missing, and we need(ed) this
                // appendix; fail.
                interp.backtrace.pop();
                return Err(error);
            }
            Err(_) => {
                // Some dependencies are missing, but we don't need this
                // appendix; don't import it.
            }
        }
    }

    // Import targets from the root file and surviving appendices, keeping
    // the component-target mapping shared across all of them.
    let mut component_targets = BTreeMap::new();
    let result = pkg.import_package_targets(interp, &file, &state.reader, &mut component_targets);
    if result.is_err() {
        interp.backtrace.pop();
        return result;
    }
    for (path, reader) in appendices {
        // `cmMakefile::CallRAII` scope for the appendix file.
        push_file_frame(interp, &path);
        let result = pkg.import_package_targets(interp, &path, &reader, &mut component_targets);
        interp.backtrace.pop();
        result?;
    }

    interp.backtrace.pop();
    Ok(())
}

/// `cmMakefile::CallRAII`: push a file context without a command.
fn push_file_frame(interp: &mut Interp, file: &str) {
    interp.backtrace.push(Frame {
        file: Rc::from(file),
        line: 0,
        name: None,
        deferred: false,
    });
}

impl Package {
    /// `cmFindPackageCommand::FindPackageDependencies`: for each
    /// requirement declared by `reader`, run a nested config+CPS-mode
    /// find_package.
    fn find_package_dependencies(
        &mut self,
        interp: &mut Interp,
        file_path: &str,
        reader: &Reader,
        required: Requirement,
    ) -> CmdResult {
        for dep in reader.requirements() {
            // `cmMakefile::CallRAII` for the file declaring the dependency.
            push_file_frame(interp, file_path);
            let mut nested = self.cps_nested(&dep, required);
            let found = nested.run(interp, &[]).is_ok() && !nested.file_found.is_empty();
            interp.backtrace.pop();
            if !found {
                return err(format!(
                    "could not find {}, required by {}.",
                    dep.name, self.name
                ));
            }
        }
        // All requirements (if any) were found.
        Ok(())
    }

    /// A nested `cmFindPackageCommand` for a CPS `requires` entry, with
    /// options inherited via `InheritOptions`.
    fn cps_nested(&self, dep: &PackageRequirement, required: Requirement) -> Package {
        let mut version = Version::default();
        version.complete.clone_from(&dep.version);
        version.min.clone_from(&dep.version);
        let components: BTreeSet<String> = dep.components.iter().cloned().collect();
        Package {
            name: dep.name.clone(),
            dir: format!("{}_DIR", dep.name),
            options: self.options,
            quiet: self.quiet,
            bypass: true,
            global: self.global,
            no_policy_scope: self.no_policy_scope,
            no_user_registry: self.no_user_registry,
            no_system_registry: self.no_system_registry,
            requirement: required,
            version,
            use_module: false,
            use_config: true,
            use_cps: true,
            components: dep.components.clone(),
            required: BTreeSet::new(),
            optional: components.clone(),
            names: Vec::new(),
            configs: Vec::new(),
            suffixes: self.suffixes.clone(),
            hints: Vec::new(),
            paths: Vec::new(),
            registry_view: self.registry_view.clone(),
            considered: Vec::new(),
            selected_version: String::new(),
            found_version_parts: None,
            debug: self.debug.is_some().then(String::new),
            required_targets: components,
            missing_targets: Vec::new(),
            file_found: String::new(),
            cps: None,
        }
    }

    /// `cmFindPackageCommand::ImportPackageTargets`: import a CPS file's
    /// components (deduplicated per found package), then apply any
    /// `<base>@*.cps` configuration supplements.
    fn import_package_targets(
        &mut self,
        interp: &mut Interp,
        file_path: &str,
        reader: &Reader,
        component_targets: &mut BTreeMap<String, String>,
    ) -> CmdResult {
        // Check if we've already imported this file.
        let file_name = filename_name(file_path);
        if interp
            .ext
            .cps_imported
            .get(&self.file_found)
            .is_some_and(|files| files.contains(&file_name))
        {
            return Ok(());
        }

        // Import base file.
        reader.import_targets(interp, component_targets)?;

        // Find supplemental configuration files.
        for extra in find_supplementals(file_path) {
            let Some(config_reader) = Reader::read(&extra, Some(reader)) else {
                continue;
            };
            if config_reader.name() == self.name {
                config_reader.import_target_configurations(interp, component_targets)?;
            }
        }

        interp
            .ext
            .cps_imported
            .entry(self.file_found.clone())
            .or_default()
            .insert(file_name);
        Ok(())
    }

    /// `cmMakefile::FindImportedTarget` checks after loading a `*.cmake`
    /// package configuration file.
    pub(super) fn check_required_targets(&mut self, interp: &Interp) {
        for target in self.required_targets.clone() {
            let qualified = format!("{}::{target}", self.name);
            if !imported_target_exists(interp, &qualified) {
                self.missing_targets.push(qualified);
            }
        }
    }
}
