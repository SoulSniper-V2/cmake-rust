//! Language enablement (port of `cmGlobalGenerator::EnableLanguage` and the
//! Ninja generator's `FindMakeProgram`). The actual compiler detection and
//! platform logic lives in CMake's own Modules, which we execute.

use std::collections::{BTreeMap, BTreeSet};

use crate::cache::CacheType;
use crate::interp::Interp;
use crate::messages::MessageType;
use crate::util;

#[derive(Debug, Default, Clone)]
pub struct LangState {
    /// Languages whose CMake<LANG>Compiler.cmake has been loaded.
    pub enabled: Vec<String>,
    pub ready: BTreeSet<String>,
    pub in_progress: BTreeSet<String>,
    /// File extension -> language.
    pub extension_to_language: BTreeMap<String, String>,
    pub ignore_extensions: BTreeSet<String>,
    pub output_extension: BTreeMap<String, String>,
    pub linker_preference: BTreeMap<String, i32>,
    /// `ninja --version`.
    pub ninja_version: Option<String>,
    /// Configured-files path when in a try_compile (outer CMakeFiles/<ver>).
    pub configured_files_path: Option<String>,
}

fn modules_file(interp: &Interp, name: &str) -> Option<String> {
    crate::commands::core::find_module(interp, name.trim_end_matches(".cmake"))
}

/// `cmMakefile::ReadListFile` used by EnableLanguage (no policy scope push
/// semantics differ slightly: these are read with a policy scope).
fn read(interp: &mut Interp, path: &str) -> bool {
    if !std::path::Path::new(path).is_file() {
        return false;
    }
    interp.read_dependent_file(path, false);
    true
}

fn set_language_enabled_flag(interp: &mut Interp, lang: &str) {
    // cmState::EnableLanguage keeps EnabledLanguages sorted; the
    // ENABLED_LANGUAGES global property is computed from it on read.
    if let Err(position) = interp
        .langs
        .enabled
        .binary_search_by(|existing| existing.as_str().cmp(lang))
    {
        interp.langs.enabled.insert(position, lang.to_owned());
    }
}

/// `cmGlobalGenerator::SetLanguageEnabledMaps`.
fn set_language_enabled_maps(interp: &mut Interp, lang: &str) {
    if lang == "NONE" {
        return;
    }
    let preference = interp
        .get_def(&format!("CMAKE_{lang}_LINKER_PREFERENCE"))
        .map(|value| util::strtol(value, 10).0 as i32)
        .unwrap_or(0);
    interp
        .langs
        .linker_preference
        .insert(lang.to_owned(), preference);
    if let Some(output) = interp
        .get_def(&format!("CMAKE_{lang}_OUTPUT_EXTENSION"))
        .map(str::to_owned)
    {
        interp
            .langs
            .output_extension
            .insert(lang.to_owned(), output.clone());
        if let Some(stripped) = output.strip_prefix('.') {
            interp
                .langs
                .output_extension
                .insert(format!("{lang}:nodot"), stripped.to_owned());
        }
    }
    if let Some(extensions) = interp
        .get_def(&format!("CMAKE_{lang}_SOURCE_FILE_EXTENSIONS"))
        .map(str::to_owned)
    {
        for extension in util::expand_list(&extensions, false) {
            interp
                .langs
                .extension_to_language
                .entry(extension)
                .or_insert_with(|| lang.to_owned());
        }
    }
    if let Some(ignores) = interp
        .get_def(&format!("CMAKE_{lang}_IGNORE_EXTENSIONS"))
        .map(str::to_owned)
    {
        for extension in util::expand_list(&ignores, false) {
            interp.langs.ignore_extensions.insert(extension);
        }
    }
}

/// `cmGlobalNinjaGenerator::FindMakeProgram`.
fn find_make_program(interp: &mut Interp) -> bool {
    let current = interp.get_def("CMAKE_MAKE_PROGRAM").map(str::to_owned);
    if current.as_deref().is_none_or(util::is_off) {
        if let Some(path) = modules_file(interp, "CMakeNinjaFindMake.cmake") {
            read(interp, &path);
        }
    }
    let program = interp.get_def("CMAKE_MAKE_PROGRAM").map(str::to_owned);
    if program.as_deref().is_none_or(util::is_off) {
        interp.issue(
            MessageType::FatalError,
            "CMake was unable to find a build program corresponding to \"Ninja\".  CMAKE_MAKE_PROGRAM is not set.  You probably need to select a different build tool.",
        );
        interp.set_fatal();
        return false;
    }
    let program = program.unwrap_or_default();
    if interp.langs.ninja_version.is_none() {
        match std::process::Command::new(&program)
            .arg("--version")
            .output()
        {
            Ok(output) if output.status.success() => {
                let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                interp.langs.ninja_version = Some(version);
            }
            _ => {
                let text = format!(
                    "Running\n '{program}' '--version'\nfailed with:\n no such file or directory"
                );
                interp.issue(MessageType::FatalError, &text);
                interp.set_fatal();
                return false;
            }
        }
    }
    true
}

/// `cmGlobalGenerator::EnableLanguage`.
pub fn enable_language(interp: &mut Interp, languages: &[String], optional: bool) {
    // cmMakefile::EnableLanguage: cmGlobalGenerator::GetCMakeCFGIntDir is "."
    // for single-configuration generators.
    interp.set_def("CMAKE_CFG_INTDIR", ".");
    if !interp.in_try_compile
        && interp.get_def("CMAKE_BUILD_TYPE").is_none()
        && let Ok(build_type) = std::env::var("CMAKE_BUILD_TYPE")
    {
        interp.add_cache_def(
            "CMAKE_BUILD_TYPE",
            Some(&build_type),
            Some("Choose the type of build.  Options include: empty, Debug, Release, RelWithDebInfo, MinSizeRel."),
            CacheType::String,
            false,
        );
    }
    if languages.is_empty() {
        crate::configure::system_error(Some(interp), "EnableLanguage must have a lang specified!");
        interp.set_fatal();
        return;
    }
    let unique: BTreeSet<String> = languages.iter().cloned().collect();
    for lang in &unique {
        if !interp.langs.in_progress.insert(lang.clone()) {
            let text = format!(
                "Language '{lang}' is currently being enabled.  Recursive call not allowed."
            );
            interp.issue(MessageType::FatalError, &text);
            interp.set_fatal();
            return;
        }
    }
    if interp.in_try_compile {
        for lang in languages {
            if lang == "NONE" {
                set_language_enabled_flag(interp, "NONE");
            } else if !interp.langs.ready.contains(lang) {
                let text = format!("The test project needs language {lang} which is not enabled.");
                interp.issue(MessageType::FatalError, &text);
                interp.set_fatal();
                return;
            }
        }
    }
    let mut fatal_error = false;
    interp.set_def("RUN_CONFIGURE", "ON");
    let mut root_bin = match &interp.langs.configured_files_path {
        Some(path) => path.clone(),
        None => format!("{}/CMakeFiles", interp.home_binary),
    };
    root_bin.push('/');
    root_bin.push_str(crate::interp::VERSION_STRING);
    interp.set_def("CMAKE_PLATFORM_INFO_DIR", root_bin.clone());
    if !interp.in_try_compile {
        if std::path::Path::new(&root_bin).is_dir()
            && !interp.is_on("CMAKE_PLATFORM_INFO_INITIALIZED")
        {
            let _ = std::fs::remove_dir_all(&root_bin);
        }
        interp.cache.add(
            "CMAKE_PLATFORM_INFO_INITIALIZED",
            Some("1"),
            Some("Platform information initialized"),
            CacheType::Internal,
        );
    }
    let read_system = interp.get_def("CMAKE_SYSTEM_LOADED").is_none();
    if read_system {
        let path = format!("{root_bin}/CMakeSystem.cmake");
        if std::path::Path::new(&path).is_file() {
            read(interp, &path);
        }
    }
    if interp.get_def("CMAKE_SYSTEM").is_none() {
        if let Some(path) = modules_file(interp, "CMakeDetermineSystem.cmake") {
            read(interp, &path);
        }
        let path = format!("{root_bin}/CMakeSystem.cmake");
        read(interp, &path);
    }
    if read_system {
        let system = interp.get_safe("CMAKE_SYSTEM_NAME");
        let _ = system;
        let platform = interp.get_safe("CMAKE_GENERATOR_PLATFORM");
        if !platform.is_empty() {
            let text = format!(
                "Generator\n  Ninja\ndoes not support platform specification, but platform\n  {platform}\nwas specified."
            );
            interp.issue(MessageType::FatalError, &text);
            interp.set_fatal();
            return;
        }
        let toolset = interp.get_safe("CMAKE_GENERATOR_TOOLSET");
        if !toolset.is_empty() {
            let text = format!(
                "Generator\n  Ninja\ndoes not support toolset specification, but toolset\n  {toolset}\nwas specified."
            );
            interp.issue(MessageType::FatalError, &text);
            interp.set_fatal();
            return;
        }
        if !find_make_program(interp) {
            return;
        }
        // CMAKE_PROJECT_TOP_LEVEL_INCLUDES
        interp.ext.in_top_level_includes = true;
        let includes = interp.get_safe("CMAKE_PROJECT_TOP_LEVEL_INCLUDES");
        for setup in util::expand_list(&includes, false) {
            let mut file = setup.clone();
            if !util::is_full_path(&file) && !file.ends_with(".cmake") {
                match modules_file(interp, &format!("{file}.cmake")) {
                    Some(path) => file = path,
                    None => {
                        crate::configure::system_error(
                            Some(interp),
                            &format!("CMAKE_PROJECT_TOP_LEVEL_INCLUDES module:\n  {setup}"),
                        );
                        interp.ext.in_top_level_includes = false;
                        return;
                    }
                }
            }
            let absolute = util::collapse_full_path(&file, &interp.current_source_dir());
            let path = std::path::Path::new(&absolute);
            if !path.exists() {
                crate::configure::system_error(
                    Some(interp),
                    &format!("CMAKE_PROJECT_TOP_LEVEL_INCLUDES file does not exist: {setup}"),
                );
                interp.ext.in_top_level_includes = false;
                return;
            }
            if path.is_dir() {
                crate::configure::system_error(
                    Some(interp),
                    &format!("CMAKE_PROJECT_TOP_LEVEL_INCLUDES file is a directory: {setup}"),
                );
                interp.ext.in_top_level_includes = false;
                return;
            }
            if !read(interp, &absolute) {
                crate::configure::system_error(
                    Some(interp),
                    &format!("Failed reading CMAKE_PROJECT_TOP_LEVEL_INCLUDES file: {setup}"),
                );
                interp.ext.in_top_level_includes = false;
                return;
            }
        }
        interp.ext.in_top_level_includes = false;
    }
    // cmGlobalNinjaGenerator::CheckFortran also accepts the pre-1.10
    // Kitware Ninja branch implementing version 1 of the dyndep feature.
    if unique.contains("Fortran")
        && let Some(version) = &interp.langs.ninja_version
        && util::version_compare(util::CompareOp::Less, version, "1.10")
        && !version
            .split_once(".dyndep-")
            .is_some_and(|(_, v)| v.parse::<u64>() == Ok(1))
    {
        let message = format!(
            "The Ninja generator does not support Fortran using Ninja version\n  {version}\ndue to lack of required features.  Ninja 1.10 or higher is required."
        );
        interp.issue(MessageType::FatalError, &message);
        interp.set_fatal();
        return;
    }
    if interp
        .get_def("CMAKE_SYSTEM_SPECIFIC_INITIALIZE_LOADED")
        .is_none()
    {
        match modules_file(interp, "CMakeSystemSpecificInitialize.cmake") {
            Some(path) => {
                read(interp, &path);
            }
            None => crate::configure::system_error(
                Some(interp),
                "Could not find cmake module file: CMakeSystemSpecificInitialize.cmake",
            ),
        }
    }
    let mut need_test: BTreeMap<String, bool> = BTreeMap::new();
    let mut need_maps: BTreeMap<String, bool> = BTreeMap::new();
    for lang in languages {
        need_maps.insert(lang.clone(), false);
        if lang == "NONE" {
            set_language_enabled_flag(interp, "NONE");
            continue;
        }
        let loaded = format!("CMAKE_{lang}_COMPILER_LOADED");
        if interp.get_def(&loaded).is_none() {
            let path = format!("{root_bin}/CMake{lang}Compiler.cmake");
            if std::path::Path::new(&path).is_file() {
                read(interp, &path);
                need_test.insert(lang.clone(), false);
                set_language_enabled_flag(interp, lang);
                need_maps.insert(lang.clone(), true);
            }
        }
        if !interp.langs.enabled.contains(lang) {
            let determine = format!("CMakeDetermine{lang}Compiler.cmake");
            match modules_file(interp, &determine) {
                Some(path) => {
                    read(interp, &path);
                }
                None => crate::configure::system_error(
                    Some(interp),
                    &format!("Could not find cmake module file: {determine}"),
                ),
            }
            if interp.fatal {
                return;
            }
            need_test.insert(lang.clone(), true);
            let path = format!("{root_bin}/CMake{lang}Compiler.cmake");
            if !read(interp, &path) {
                crate::configure::system_error(
                    Some(interp),
                    &format!("Could not find cmake module file: {path}"),
                );
            }
            set_language_enabled_flag(interp, lang);
            need_maps.insert(lang.clone(), true);
        }
    }
    if interp
        .get_def("CMAKE_SYSTEM_SPECIFIC_INFORMATION_LOADED")
        .is_none()
    {
        match modules_file(interp, "CMakeSystemSpecificInformation.cmake") {
            Some(path) => {
                read(interp, &path);
            }
            None => crate::configure::system_error(
                Some(interp),
                "Could not find cmake module file: CMakeSystemSpecificInformation.cmake",
            ),
        }
    }
    for lang in languages {
        if lang == "NONE" {
            set_language_enabled_flag(interp, "NONE");
            continue;
        }
        let compiler_name = format!("CMAKE_{lang}_COMPILER");
        let compiler_env = format!("CMAKE_{lang}_COMPILER_ENV_VAR");
        let compiler = interp.get_def(&compiler_name).map(str::to_owned);
        let mut no_compiler = String::new();
        match &compiler {
            Some(file) if !file.is_empty() && !util::is_notfound(file) => {
                if lang != "RC" && lang != "ASM_MARMASM" && lang != "ASM_MASM" {
                    if !util::is_full_path(file) {
                        no_compiler = format!(
                            "The {compiler_name}:\n  {file}\nis not a full path and was not found in the PATH.\n"
                        );
                    } else if !std::path::Path::new(file).exists() {
                        no_compiler = format!(
                            "The {compiler_name}:\n  {file}\nis not a full path to an existing compiler tool.\n"
                        );
                    }
                }
            }
            _ => no_compiler = format!("No {compiler_name} could be found.\n"),
        }
        if !no_compiler.is_empty() {
            need_test.insert(lang.clone(), false);
            if !optional {
                let _ = std::fs::remove_file(format!("{root_bin}/CMake{lang}Compiler.cmake"));
                if !interp.in_try_compile {
                    let env_var = interp.get_def(&compiler_env).map(str::to_owned);
                    let advice = compiler_advice(lang, env_var.as_deref());
                    let text = format!("{no_compiler}{advice}");
                    interp.issue(MessageType::FatalError, &text);
                    fatal_error = true;
                }
            }
        }
        let loaded = format!("CMAKE_{lang}_INFORMATION_LOADED");
        if interp.get_def(&loaded).is_none() {
            let file = format!("CMake{lang}Information.cmake");
            match modules_file(interp, &file) {
                Some(path) => {
                    if !read(interp, &path) {
                        crate::configure::system_error(
                            Some(interp),
                            &format!("Could not process cmake module file: {path}"),
                        );
                    }
                }
                None => crate::configure::system_error(
                    Some(interp),
                    &format!("Could not find cmake module file: {file}"),
                ),
            }
        }
        if need_maps.get(lang).copied().unwrap_or(false) {
            set_language_enabled_maps(interp, lang);
        }
        interp.langs.ready.insert(lang.clone());
        if need_test.get(lang).copied().unwrap_or(false) && !interp.in_try_compile {
            let file = format!("CMakeTest{lang}Compiler.cmake");
            match modules_file(interp, &file) {
                Some(path) => {
                    read(interp, &path);
                }
                None => crate::configure::system_error(
                    Some(interp),
                    &format!("Could not find cmake module file: {file}"),
                ),
            }
            if !interp.is_on(&format!("CMAKE_{lang}_COMPILER_WORKS")) {
                let _ = std::fs::remove_file(format!("{root_bin}/CMake{lang}Compiler.cmake"));
            }
        }
        if interp.is_on(&format!("CMAKE_{lang}_COMPILER_WORKS"))
            && interp.is_on(&format!("CMAKE_{lang}_USE_LINKER_INFORMATION"))
        {
            let loaded = format!("CMAKE_{lang}_LINKER_INFORMATION_LOADED");
            if interp.get_def(&loaded).is_none() {
                let file = format!("CMake{lang}LinkerInformation.cmake");
                let path = modules_file(interp, &file)
                    .or_else(|| modules_file(interp, &format!("Internal/{file}")));
                match path {
                    Some(path) => {
                        read(interp, &path);
                    }
                    None => crate::configure::system_error(
                        Some(interp),
                        &format!("Could not find cmake module file: {file}"),
                    ),
                }
            }
            if need_test.get(lang).copied().unwrap_or(false) && !interp.in_try_compile {
                let file = format!("Internal/CMakeInspect{lang}Linker.cmake");
                if let Some(path) = modules_file(interp, &file) {
                    read(interp, &path);
                } else {
                    crate::configure::system_error(
                        Some(interp),
                        &format!("Could not find cmake module file: {file}"),
                    );
                }
            }
        }
        check_compiler_id_compatibility(interp, lang);
    }
    let project_compat = format!(
        "{}/Modules/{}Compatibility.cmake",
        interp.cmake_root,
        interp.get_safe("PROJECT_NAME")
    );
    if std::path::Path::new(&project_compat).is_file() {
        read(interp, &project_compat);
    }
    if fatal_error {
        interp.set_fatal();
    }
    for lang in &unique {
        interp.langs.in_progress.remove(lang);
    }
}

/// `cmGlobalGenerator::PrintCompilerAdvice`.
fn compiler_advice(lang: &str, env_var: Option<&str>) -> String {
    let mut advice = String::from("Tell CMake where to find the compiler by setting ");
    if let Some(var) = env_var {
        advice.push_str(&format!("either the environment variable \"{var}\" or "));
    }
    advice.push_str(&format!(
        "the CMake cache entry CMAKE_{lang}_COMPILER to the full path to the compiler, or to the compiler name if it is in the PATH."
    ));
    advice
}

/// `cmGlobalGenerator::CheckCompilerIdCompatibility` (AppleClang/QCC/etc.).
fn check_compiler_id_compatibility(interp: &mut Interp, lang: &str) {
    let id_var = format!("CMAKE_{lang}_COMPILER_ID");
    let id = interp.get_safe(&id_var);
    if id == "XLClang" {
        // Handled by policies removed in 4.x; nothing to do.
    }
    let _ = id;
}
