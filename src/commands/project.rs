//! project() and enable_language() (ports of cmProjectCommand.cxx and
//! cmEnableLanguageCommand.cxx).

use crate::cache::CacheType;
use crate::interp::{CmdErr, CmdResult, Interp, err};
use crate::messages::MessageType;
use crate::policy::PolicyStatus;
use crate::util;

pub fn register(interp: &mut Interp) {
    interp.register("project", project);
    interp.register("enable_language", enable_language);
}

fn is_root(interp: &Interp) -> bool {
    interp.mf_index() == 0
}

const KEYWORDS: [&str; 4] = ["VERSION", "DESCRIPTION", "HOMEPAGE_URL", "LANGUAGES"];
const COMPAT_VERSION_KEYWORD: &str = "COMPAT_VERSION";
/// Activation UUID for `cmExperimental::Feature::ExportPackageInfo`.
const EXPORT_PACKAGE_INFO_UUID: &str = "b80be207-778e-46ba-8080-b23bba22639e";
const EXPORT_PACKAGE_INFO_VARIABLE: &str = "CMAKE_EXPERIMENTAL_EXPORT_PACKAGE_INFO";
const EXPORT_PACKAGE_INFO_DESCRIPTION: &str = "CMake's support for exporting package information in the Common Package Specification format is experimental. It is meant only for experimentation and feedback to CMake developers.";

/// `cmExperimental::HasSupportEnabled(Feature::ExportPackageInfo)`.
fn export_package_info_enabled(interp: &mut Interp) -> bool {
    let Some(value) = interp.read_definition(EXPORT_PACKAGE_INFO_VARIABLE) else {
        return false;
    };
    let enabled = value == EXPORT_PACKAGE_INFO_UUID;
    // `cmGlobalGenerator::ShouldWarnExperimental`: warn once per
    // feature-name/value pair.
    if interp
        .ext
        .experimental_warnings
        .insert(("ExportPackageInfo".to_owned(), value.clone()))
    {
        let warning = if enabled {
            EXPORT_PACKAGE_INFO_DESCRIPTION.to_owned()
        } else {
            format!("{EXPORT_PACKAGE_INFO_VARIABLE} is set to incorrect value\n  {value}\nSee 'Help/dev/experimental.rst' in the source tree of this version of CMake for documentation of the experimental feature and the corresponding activation value.  This project's code may require changes to work with this CMake's version of the feature.")
        };
        interp.issue(MessageType::AuthorWarning, &warning);
    }
    enabled
}

/// The `vx` regular expression in cmProjectCommand.cxx:
/// `^([0-9]+(\.[0-9]+(\.[0-9]+(\.[0-9]+)?)?)?)?$`
fn valid_version(version: &str) -> bool {
    if version.is_empty() {
        return true;
    }
    let parts: Vec<&str> = version.split('.').collect();
    parts.len() <= 4
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

/// `cmMakefile::EnableLanguage`: reports duplicate languages, moves RC last,
/// and only invokes the global generator for non-empty language sets.
fn enable_languages(interp: &mut Interp, languages: &[String], optional: bool) {
    if interp.ext.defer_current_id.is_some() {
        interp.issue(
            MessageType::FatalError,
            "Languages may not be enabled during deferred execution.",
        );
        interp.set_fatal();
        return;
    }
    if languages.is_empty() {
        return;
    }
    let mut unique: Vec<&str> = Vec::new();
    let mut duplicates: Vec<&str> = Vec::new();
    for language in languages {
        let language = language.as_str();
        if !unique.contains(&language) {
            unique.push(language);
        } else if !duplicates.contains(&language) {
            duplicates.push(language);
        }
    }
    if !duplicates.is_empty() {
        let quantity = if duplicates.len() == 1 { " has" } else { "s have" };
        let text = format!(
            "Languages to be enabled may not be specified more than once at the same time. The following language{quantity} been specified multiple times: {}",
            duplicates.join(", ")
        );
        interp.issue(MessageType::AuthorWarning, &text);
    }
    // If RC is explicitly listed we need to do it after other languages.
    let mut without_rc: Vec<String> = Vec::new();
    let mut for_rc: Vec<String> = Vec::new();
    for language in unique {
        if language == "RC" {
            for_rc.push(language.to_owned());
        } else {
            without_rc.push(language.to_owned());
        }
    }
    if !without_rc.is_empty() {
        crate::languages::enable_language(interp, &without_rc, optional);
    }
    if !for_rc.is_empty() {
        crate::languages::enable_language(interp, &for_rc, optional);
    }
}

/// TopLevelCMakeVarCondSet.
fn top_level_set(interp: &mut Interp, name: &str, value: &str) {
    if interp.get_def(name).is_none() || is_root(interp) {
        interp.unset_def(name);
        interp.add_cache_def(name, Some(value), Some("Value Computed by CMake"), CacheType::Static, false);
    }
}

fn include_by_variable(interp: &mut Interp, variable: &str) -> CmdResult {
    let Some(include) = interp.get_def(variable).map(str::to_owned) else {
        return Ok(());
    };
    let mut failure: Option<String> = None;
    for mut file in util::expand_list(&include, false) {
        if !util::is_full_path(&file) && !file.ends_with(".cmake") {
            match crate::commands::core::find_module(interp, &file) {
                Some(path) => file = path,
                None => {
                    failure = Some(format!("could not find requested module:\n  {file}"));
                    continue;
                }
            }
        }
        let full = util::collapse_full_path(&file, &interp.current_source_dir());
        let path = std::path::Path::new(&full);
        if !path.exists() {
            failure = Some(format!("could not find requested file:\n  {file}"));
            continue;
        }
        if path.is_dir() {
            failure = Some(format!("requested file is a directory:\n  {file}"));
            continue;
        }
        if !interp.read_dependent_file(&full, false) && !interp.fatal {
            failure = Some(format!("could not load requested file:\n  {file}"));
        }
    }
    match failure {
        Some(message) => err(message),
        None => Ok(()),
    }
}

fn project(interp: &mut Interp, args: &[String]) -> CmdResult {
    // cmExperimental::HasSupportEnabled is queried before the argument check
    // so that COMPAT_VERSION is only a keyword when the feature is enabled.
    let compat_enabled = export_package_info_enabled(interp);
    if args.is_empty() {
        return err("PROJECT called with incorrect number of arguments");
    }
    let name = args[0].clone();
    if KEYWORDS.contains(&name.as_str()) || (compat_enabled && name == COMPAT_VERSION_KEYWORD) {
        let text = format!(
            "project() called with '{name}' as first argument. The first parameter should be the project name, not a keyword argument. See the cmake-commands(7) manual for correct usage of the project() command."
        );
        interp.issue(MessageType::AuthorWarning, &text);
    }
    // cmArgumentParser semantics. Keyword-bound optionals are "engaged" as
    // soon as the keyword is parsed, even if no value follows.
    let mut version: Option<String> = None;
    let mut description: Option<String> = None;
    let mut homepage: Option<String> = None;
    let mut compat_version: Option<String> = None;
    let mut languages: Option<Vec<String>> = None;
    let mut unparsed: Vec<String> = Vec::new();
    let mut parsed_keywords: Vec<&'static str> = Vec::new();
    let mut missing: Vec<&'static str> = Vec::new();
    let mut current: Option<&'static str> = None;
    let mut index = 1;
    while index < args.len() {
        let arg = args[index].as_str();
        let keyword = KEYWORDS
            .iter()
            .copied()
            .find(|keyword| *keyword == arg)
            .or_else(|| {
                (compat_enabled && arg == COMPAT_VERSION_KEYWORD)
                    .then_some(COMPAT_VERSION_KEYWORD)
            });
        if let Some(keyword) = keyword {
            if let Some(previous) = current
                && previous != "LANGUAGES"
            {
                missing.push(previous);
            }
            parsed_keywords.push(keyword);
            current = Some(keyword);
            match keyword {
                "VERSION" => version = Some(String::new()),
                "DESCRIPTION" => description = Some(String::new()),
                "HOMEPAGE_URL" => homepage = Some(String::new()),
                "COMPAT_VERSION" => compat_version = Some(String::new()),
                "LANGUAGES" => {
                    languages.get_or_insert_with(Vec::new);
                }
                _ => {}
            }
            index += 1;
            continue;
        }
        match current {
            Some("VERSION") => {
                *version.as_mut().expect("version") = arg.to_owned();
                current = None;
            }
            Some("DESCRIPTION") => {
                *description.as_mut().expect("description") = arg.to_owned();
                current = None;
            }
            Some("HOMEPAGE_URL") => {
                *homepage.as_mut().expect("homepage") = arg.to_owned();
                current = None;
            }
            Some("COMPAT_VERSION") => {
                *compat_version.as_mut().expect("compat_version") = arg.to_owned();
                current = None;
            }
            Some("LANGUAGES") => languages.as_mut().expect("languages").push(arg.to_owned()),
            _ => unparsed.push(arg.to_owned()),
        }
        index += 1;
    }
    if let Some(previous) = current
        && previous != "LANGUAGES"
    {
        missing.push(previous);
    }
    if is_root(interp) && interp.get_def("CMAKE_MINIMUM_REQUIRED_VERSION").is_none() {
        interp.issue(
            MessageType::AuthorWarning,
            "cmake_minimum_required() should be called prior to this top-level project() call. Please see the cmake-commands(7) manual for usage documentation of both commands.",
        );
    }
    include_by_variable(interp, "CMAKE_PROJECT_INCLUDE_BEFORE")?;
    include_by_variable(interp, &format!("CMAKE_PROJECT_{name}_INCLUDE_BEFORE"))?;
    interp.mf_mut().props.set("PROJECT_NAME", name.clone());
    let cmp0180 = interp.policy_status("CMP0180");
    let binary = interp.current_binary_dir();
    let source = interp.current_source_dir();
    for (suffix, value) in [("_BINARY_DIR", binary.clone()), ("_SOURCE_DIR", source.clone())] {
        let variable = format!("{name}{suffix}");
        let already = interp.get_normal_def(&variable).is_some();
        interp.add_cache_def(&variable, Some(&value), Some("Value Computed by CMake"), CacheType::Static, false);
        if cmp0180 == PolicyStatus::New || already {
            interp.set_def(variable, value);
        }
    }
    interp.set_def("PROJECT_BINARY_DIR", binary);
    interp.set_def("PROJECT_SOURCE_DIR", source);
    interp.set_def("PROJECT_NAME", name.clone());
    let root = is_root(interp);
    interp.set_def("PROJECT_IS_TOP_LEVEL", if root { "ON" } else { "OFF" });
    let variable = format!("{name}_IS_TOP_LEVEL");
    let already = interp.get_normal_def(&variable).is_some();
    interp.add_cache_def(
        &variable,
        Some(if root { "ON" } else { "OFF" }),
        Some("Value Computed by CMake"),
        CacheType::Static,
        false,
    );
    if cmp0180 == PolicyStatus::New || already {
        interp.set_def(variable, if root { "ON" } else { "OFF" });
    }
    top_level_set(interp, "CMAKE_PROJECT_NAME", &name);
    let mut seen: Vec<&str> = Vec::new();
    for keyword in &parsed_keywords {
        if seen.contains(keyword) {
            let text = format!("{keyword} may be specified at most once.");
            interp.issue(MessageType::FatalError, &text);
            interp.set_fatal();
            return Ok(());
        }
        seen.push(keyword);
    }
    for keyword in &missing {
        let text = format!(
            "{keyword} keyword not followed by a value or was followed by a value that expanded to nothing."
        );
        interp.issue(MessageType::Warning, &text);
    }
    if !unparsed.is_empty() {
        if languages.is_some() {
            let text = format!(
                "the following parameters must be specified after LANGUAGES keyword: {}.",
                unparsed.join(", ")
            );
            interp.issue(MessageType::Warning, &text);
        } else if version.is_some() || description.is_some() || homepage.is_some() {
            interp.issue(
                MessageType::FatalError,
                "project with VERSION, DESCRIPTION or HOMEPAGE_URL must use LANGUAGES before language names.",
            );
            interp.set_fatal();
            return Ok(());
        }
    } else if languages.as_ref().is_some_and(Vec::is_empty) {
        languages = Some(vec!["NONE".to_owned()]);
    }
    if compat_version.is_some() && version.is_none() {
        interp.issue(
            MessageType::FatalError,
            "project with COMPAT_VERSION must also provide VERSION.",
        );
        interp.set_fatal();
        return Ok(());
    }
    let mut version_string = String::new();
    let mut components: [String; 4] = Default::default();
    if let Some(version) = &version {
        if !valid_version(version) {
            let text = format!("VERSION \"{version}\" format invalid.");
            interp.issue(MessageType::FatalError, &text);
            interp.set_fatal();
            return Ok(());
        }
        let cmp0096 = interp.policy_status("CMP0096");
        if cmp0096 == PolicyStatus::New {
            version_string = version.clone();
            for (index, part) in version.split('.').enumerate() {
                if index < 4 {
                    components[index] = part.to_owned();
                }
            }
            if version.is_empty() {
                components[0].clear();
            }
        } else {
            let (parts, count) = crate::commands::core::scan_version(version);
            for index in 0..count.min(4) {
                if index > 0 {
                    version_string.push('.');
                }
                version_string.push_str(&parts[index].to_string());
                components[index] = parts[index].to_string();
            }
        }
    }
    if let Some(compat_version) = &compat_version {
        if !valid_version(compat_version) {
            let text = format!("COMPAT_VERSION \"{compat_version}\" format invalid.");
            interp.issue(MessageType::FatalError, &text);
            interp.set_fatal();
            return Ok(());
        }
        if util::version_compare(util::CompareOp::Greater, compat_version, &version_string) {
            interp.issue(
                MessageType::FatalError,
                "COMPAT_VERSION must be less than or equal to VERSION",
            );
            interp.set_fatal();
            return Ok(());
        }
    }
    let create = |interp: &mut Interp, variable: &str, value: &str| {
        interp.set_def(format!("PROJECT_{variable}"), value);
        interp.set_def(format!("{name}_{variable}"), value);
        top_level_set(interp, &format!("CMAKE_PROJECT_{variable}"), value);
    };
    create(interp, "VERSION", &version_string);
    create(interp, "VERSION_MAJOR", &components[0].clone());
    create(interp, "VERSION_MINOR", &components[1].clone());
    create(interp, "VERSION_PATCH", &components[2].clone());
    create(interp, "VERSION_TWEAK", &components[3].clone());
    create(interp, "COMPAT_VERSION", compat_version.as_deref().unwrap_or(""));
    create(interp, "DESCRIPTION", description.as_deref().unwrap_or(""));
    create(interp, "HOMEPAGE_URL", homepage.as_deref().unwrap_or(""));
    if unparsed.is_empty() && languages.is_none() {
        // if no language is specified do c and c++
        enable_languages(interp, &["C".to_owned(), "CXX".to_owned()], false);
    } else {
        if !unparsed.is_empty() {
            enable_languages(interp, &unparsed, false);
        }
        if let Some(languages) = &languages {
            enable_languages(interp, languages, false);
        }
    }
    include_by_variable(interp, "CMAKE_PROJECT_INCLUDE")?;
    include_by_variable(interp, &format!("CMAKE_PROJECT_{name}_INCLUDE"))?;
    Ok(())
}

/// `cmEnableLanguageCommand`.
fn enable_language(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.is_empty() {
        return err("called with incorrect number of arguments");
    }
    if interp.get_normal_def("PROJECT_NAME").is_none() {
        match interp.policy_status("CMP0165") {
            PolicyStatus::Warn => interp.issue(
                MessageType::AuthorWarning,
                "project() should be called prior to this enable_language() call.",
            ),
            PolicyStatus::New => {
                interp.issue(
                    MessageType::FatalError,
                    "project() must be called prior to this enable_language() call.",
                );
                interp.set_fatal();
                return Err(CmdErr::Reported);
            }
            _ => {}
        }
    }
    let mut optional = false;
    let mut languages = Vec::new();
    for arg in args {
        if arg == "OPTIONAL" {
            optional = true;
        } else {
            languages.push(arg.clone());
        }
    }
    enable_languages(interp, &languages, optional);
    if interp.fatal {
        return Err(CmdErr::Reported);
    }
    Ok(())
}
