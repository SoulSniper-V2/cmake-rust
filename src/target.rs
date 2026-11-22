//! Configure-time target and source semantics from cmTarget and cmSourceFile.

use std::borrow::Cow;
use std::path::Path;

use crate::interp::{CmdErr, CmdResult, Interp, err};
use crate::messages::{Frame, MessageType};
use crate::policy::{self, PolicyStatus};
use crate::state::{
    PropertyEntry, SourceFile, SourceFileLocation, Target, TargetKind, find_alias,
    find_global_target_key, find_target_key, find_target_to_use,
};
use crate::util::{collapse_full_path, expand_list, is_on, relative_if_under};

#[derive(Clone, Copy)]
enum Condition {
    Always,
    ExecutableTarget,
    CanCompileSources,
    TargetWithArtifact,
    NonImportedTarget,
    SharedLibraryTarget,
    TargetWithCommands,
    TargetWithSymbolExports,
    NormalNonImportedTarget,
    NonExecutableWithArtifact,
    LinkableLibraryTarget,
    NormalTarget,
    NeedsXcode,
    #[cfg(target_os = "macos")]
    NeedsXcodeAndCanCompileSources,
}

#[derive(Clone, Copy)]
enum Repeat {
    Once,
    PerConfig,
    PerConfigPrefix,
}

// cmTarget.cxx: StaticTargetProperties.
#[rustfmt::skip]
const INITIALIZERS: &[(&str, Condition, Repeat, Option<&str>)] = &{
    use Condition::*;
    use Repeat::*;
    [
        // -- Debugger Properties
        ("DEBUGGER_WORKING_DIRECTORY", ExecutableTarget, Once, None),
        // Compilation properties
        ("COMPILE_WARNING_AS_ERROR", CanCompileSources, Once, None),
        ("INTERPROCEDURAL_OPTIMIZATION", CanCompileSources, Once, None),
        ("INTERPROCEDURAL_OPTIMIZATION_", TargetWithArtifact, PerConfig, None),
        ("NO_SYSTEM_FROM_IMPORTED", CanCompileSources, Once, None),
        // Set to `True` for `SHARED` and `MODULE` targets.
        ("POSITION_INDEPENDENT_CODE", CanCompileSources, Once, None),
        ("VISIBILITY_INLINES_HIDDEN", CanCompileSources, Once, None),
        // -- Features
        // ---- PCH
        ("DISABLE_PRECOMPILE_HEADERS", CanCompileSources, Once, None),
        ("PCH_WARN_INVALID", CanCompileSources, Once, Some("ON")),
        ("PCH_INSTANTIATE_TEMPLATES", CanCompileSources, Once, Some("ON")),
        // -- Platforms
        // ---- Android
        ("ANDROID_API", CanCompileSources, Once, None),
        ("ANDROID_API_MIN", CanCompileSources, Once, None),
        ("ANDROID_ARCH", CanCompileSources, Once, None),
        ("ANDROID_ASSETS_DIRECTORIES", CanCompileSources, Once, None),
        ("ANDROID_JAVA_SOURCE_DIR", CanCompileSources, Once, None),
        ("ANDROID_STL_TYPE", CanCompileSources, Once, None),
        // ---- macOS
        ("OSX_ARCHITECTURES", CanCompileSources, Once, None),
        // ---- Windows
        ("MSVC_DEBUG_INFORMATION_FORMAT", CanCompileSources, Once, None),
        ("MSVC_RUNTIME_CHECKS", CanCompileSources, Once, None),
        ("MSVC_RUNTIME_LIBRARY", CanCompileSources, Once, None),
        ("VS_JUST_MY_CODE_DEBUGGING", CanCompileSources, Once, None),
        ("VS_DEBUGGER_COMMAND", ExecutableTarget, Once, None),
        ("VS_DEBUGGER_COMMAND_ARGUMENTS", ExecutableTarget, Once, None),
        ("VS_DEBUGGER_ENVIRONMENT", ExecutableTarget, Once, None),
        ("VS_DEBUGGER_WORKING_DIRECTORY", ExecutableTarget, Once, None),
        ("VS_USE_DEBUG_LIBRARIES", NonImportedTarget, Once, None),
        // ---- OpenWatcom
        ("WATCOM_RUNTIME_LIBRARY", CanCompileSources, Once, None),
        // ---- AIX
        ("AIX_SHARED_LIBRARY_ARCHIVE", SharedLibraryTarget, Once, None),
        // -- Language
        // ---- C
        ("C_COMPILER_LAUNCHER", CanCompileSources, Once, None),
        ("C_STANDARD", CanCompileSources, Once, None),
        ("C_STANDARD_REQUIRED", CanCompileSources, Once, None),
        ("C_EXTENSIONS", CanCompileSources, Once, None),
        ("C_VISIBILITY_PRESET", CanCompileSources, Once, None),
        // ---- C++
        ("CXX_COMPILER_LAUNCHER", CanCompileSources, Once, None),
        ("CXX_STANDARD", CanCompileSources, Once, None),
        ("CXX_STANDARD_REQUIRED", CanCompileSources, Once, None),
        ("CXX_EXTENSIONS", CanCompileSources, Once, None),
        ("CXX_VISIBILITY_PRESET", CanCompileSources, Once, None),
        ("CXX_MODULE_STD", CanCompileSources, Once, None),
        // ---- CSharp
        ("DOTNET_SDK", NonImportedTarget, Once, None),
        ("DOTNET_TARGET_FRAMEWORK", TargetWithCommands, Once, None),
        ("DOTNET_TARGET_FRAMEWORK_VERSION", TargetWithCommands, Once, None),
        // ---- CUDA
        ("CUDA_COMPILER_LAUNCHER", CanCompileSources, Once, None),
        ("CUDA_STANDARD", CanCompileSources, Once, None),
        ("CUDA_STANDARD_REQUIRED", CanCompileSources, Once, None),
        ("CUDA_EXTENSIONS", CanCompileSources, Once, None),
        ("CUDA_VISIBILITY_PRESET", CanCompileSources, Once, None),
        ("CUDA_SEPARABLE_COMPILATION", CanCompileSources, Once, None),
        ("CUDA_ARCHITECTURES", CanCompileSources, Once, None),
        // ---- Fortran
        ("Fortran_FORMAT", CanCompileSources, Once, None),
        ("Fortran_MODULE_DIRECTORY", CanCompileSources, Once, None),
        ("Fortran_COMPILER_LAUNCHER", CanCompileSources, Once, None),
        ("Fortran_PREPROCESS", CanCompileSources, Once, None),
        ("Fortran_VISIBILITY_PRESET", CanCompileSources, Once, None),
        // ---- HIP
        ("HIP_COMPILER_LAUNCHER", CanCompileSources, Once, None),
        ("HIP_STANDARD", CanCompileSources, Once, None),
        ("HIP_STANDARD_REQUIRED", CanCompileSources, Once, None),
        ("HIP_EXTENSIONS", CanCompileSources, Once, None),
        ("HIP_VISIBILITY_PRESET", CanCompileSources, Once, None),
        ("HIP_ARCHITECTURES", CanCompileSources, Once, None),
        // ---- ISPC
        ("ISPC_COMPILER_LAUNCHER", CanCompileSources, Once, None),
        ("ISPC_HEADER_DIRECTORY", CanCompileSources, Once, None),
        ("ISPC_HEADER_SUFFIX", CanCompileSources, Once, Some("_ispc.h")),
        ("ISPC_INSTRUCTION_SETS", CanCompileSources, Once, None),
        // ---- Objective C
        ("OBJC_COMPILER_LAUNCHER", CanCompileSources, Once, None),
        ("OBJC_STANDARD", CanCompileSources, Once, None),
        ("OBJC_STANDARD_REQUIRED", CanCompileSources, Once, None),
        ("OBJC_EXTENSIONS", CanCompileSources, Once, None),
        ("OBJC_VISIBILITY_PRESET", CanCompileSources, Once, None),
        // ---- Objective C++
        ("OBJCXX_COMPILER_LAUNCHER", CanCompileSources, Once, None),
        ("OBJCXX_STANDARD", CanCompileSources, Once, None),
        ("OBJCXX_STANDARD_REQUIRED", CanCompileSources, Once, None),
        ("OBJCXX_EXTENSIONS", CanCompileSources, Once, None),
        ("OBJCXX_VISIBILITY_PRESET", CanCompileSources, Once, None),
        // ---- Swift
        ("Swift_LANGUAGE_VERSION", CanCompileSources, Once, None),
        ("Swift_MODULE_DIRECTORY", CanCompileSources, Once, None),
        ("Swift_COMPILATION_MODE", CanCompileSources, Once, None),
        // ---- moc
        ("AUTOMOC", CanCompileSources, Once, None),
        ("AUTOMOC_COMPILER_PREDEFINES", CanCompileSources, Once, None),
        ("AUTOMOC_INCLUDE_DIRECTORIES", CanCompileSources, Once, None),
        ("AUTOMOC_MACRO_NAMES", CanCompileSources, Once, None),
        ("AUTOMOC_MOC_OPTIONS", CanCompileSources, Once, None),
        ("AUTOMOC_PATH_PREFIX", CanCompileSources, Once, None),
        ("AUTOMOC_EXECUTABLE", CanCompileSources, Once, None),
        // ---- uic
        ("AUTOUIC", CanCompileSources, Once, None),
        ("AUTOUIC_OPTIONS", CanCompileSources, Once, None),
        ("AUTOUIC_SEARCH_PATHS", CanCompileSources, Once, None),
        ("AUTOUIC_EXECUTABLE", CanCompileSources, Once, None),
        // ---- rcc
        ("AUTORCC", CanCompileSources, Once, None),
        ("AUTORCC_OPTIONS", CanCompileSources, Once, None),
        ("AUTORCC_EXECUTABLE", CanCompileSources, Once, None),
        // Linking properties
        ("LINKER_TYPE", CanCompileSources, Once, None),
        ("LINK_WARNING_AS_ERROR", CanCompileSources, Once, None),
        ("ENABLE_EXPORTS", TargetWithSymbolExports, Once, None),
        ("LINK_LIBRARIES_ONLY_TARGETS", NormalNonImportedTarget, Once, None),
        ("LINK_LIBRARIES_STRATEGY", NormalNonImportedTarget, Once, None),
        ("LINK_SEARCH_START_STATIC", CanCompileSources, Once, None),
        ("LINK_SEARCH_END_STATIC", CanCompileSources, Once, None),
        // Initialize per-configuration name postfix property from the variable only
        // for non-executable targets.  This preserves compatibility with previous
        // CMake versions in which executables did not support this variable.
        // Projects may still specify the property directly.
        ("_POSTFIX", NonExecutableWithArtifact, PerConfigPrefix, None),
        // -- Dependent library lookup
        ("MACOSX_RPATH", CanCompileSources, Once, None),
        // ---- Build
        ("BUILD_RPATH", CanCompileSources, Once, None),
        ("BUILD_RPATH_USE_ORIGIN", CanCompileSources, Once, None),
        ("SKIP_BUILD_RPATH", CanCompileSources, Once, Some("OFF")),
        ("BUILD_WITH_INSTALL_RPATH", CanCompileSources, Once, Some("OFF")),
        ("BUILD_WITH_INSTALL_NAME_DIR", CanCompileSources, Once, None),
        // ---- Install
        ("INSTALL_NAME_DIR", CanCompileSources, Once, None),
        ("INSTALL_REMOVE_ENVIRONMENT_RPATH", CanCompileSources, Once, None),
        ("INSTALL_RPATH", CanCompileSources, Once, Some("")),
        ("INSTALL_RPATH_USE_LINK_PATH", CanCompileSources, Once, Some("OFF")),
        // -- Platforms
        // ---- AIX
        ("AIX_EXPORT_ALL_SYMBOLS", TargetWithSymbolExports, Once, None),
        // ---- Android
        ("ANDROID_GUI", ExecutableTarget, Once, None),
        ("ANDROID_JAR_DIRECTORIES", CanCompileSources, Once, None),
        ("ANDROID_JAR_DEPENDENCIES", CanCompileSources, Once, None),
        ("ANDROID_NATIVE_LIB_DIRECTORIES", CanCompileSources, Once, None),
        ("ANDROID_NATIVE_LIB_DEPENDENCIES", CanCompileSources, Once, None),
        ("ANDROID_PROGUARD", CanCompileSources, Once, None),
        ("ANDROID_PROGUARD_CONFIG_PATH", CanCompileSources, Once, None),
        ("ANDROID_SECURE_PROPS_PATH", CanCompileSources, Once, None),
        // ---- iOS
        ("IOS_INSTALL_COMBINED", CanCompileSources, Once, None),
        // ---- macOS
        ("FRAMEWORK_MULTI_CONFIG_POSTFIX_", LinkableLibraryTarget, PerConfig, None),
        // ---- Windows
        ("DLL_NAME_WITH_SOVERSION", SharedLibraryTarget, Once, None),
        ("GNUtoMS", CanCompileSources, Once, None),
        ("WIN32_EXECUTABLE", CanCompileSources, Once, None),
        ("WINDOWS_EXPORT_ALL_SYMBOLS", TargetWithSymbolExports, Once, None),
        // -- Languages
        // ---- C
        ("C_LINKER_LAUNCHER", CanCompileSources, Once, None),
        // ---- C++
        ("CXX_LINKER_LAUNCHER", CanCompileSources, Once, None),
        // ---- CUDA
        ("CUDA_LINKER_LAUNCHER", CanCompileSources, Once, None),
        ("CUDA_RESOLVE_DEVICE_SYMBOLS", CanCompileSources, Once, None),
        ("CUDA_RUNTIME_LIBRARY", CanCompileSources, Once, None),
        // ---- HIP
        ("HIP_LINKER_LAUNCHER", CanCompileSources, Once, None),
        ("HIP_RUNTIME_LIBRARY", CanCompileSources, Once, None),
        // ---- Objective C
        ("OBJC_LINKER_LAUNCHER", CanCompileSources, Once, None),
        // ---- Objective C++
        ("OBJCXX_LINKER_LAUNCHER", CanCompileSources, Once, None),
        // ---- Fortran
        ("Fortran_LINKER_LAUNCHER", CanCompileSources, Once, None),
        // Static analysis
        // -- C
        ("C_CLANG_TIDY", CanCompileSources, Once, None),
        ("C_CLANG_TIDY_EXPORT_FIXES_DIR", CanCompileSources, Once, None),
        ("C_CPPLINT", CanCompileSources, Once, None),
        ("C_CPPCHECK", CanCompileSources, Once, None),
        ("C_ICSTAT", CanCompileSources, Once, None),
        ("C_INCLUDE_WHAT_YOU_USE", CanCompileSources, Once, None),
        // -- C++
        ("CXX_CLANG_TIDY", CanCompileSources, Once, None),
        ("CXX_CLANG_TIDY_EXPORT_FIXES_DIR", CanCompileSources, Once, None),
        ("CXX_CPPLINT", CanCompileSources, Once, None),
        ("CXX_CPPCHECK", CanCompileSources, Once, None),
        ("CXX_ICSTAT", CanCompileSources, Once, None),
        ("CXX_INCLUDE_WHAT_YOU_USE", CanCompileSources, Once, None),
        // -- Objective C
        ("OBJC_CLANG_TIDY", CanCompileSources, Once, None),
        ("OBJC_CLANG_TIDY_EXPORT_FIXES_DIR", CanCompileSources, Once, None),
        // -- Objective C++
        ("OBJCXX_CLANG_TIDY", CanCompileSources, Once, None),
        ("OBJCXX_CLANG_TIDY_EXPORT_FIXES_DIR", CanCompileSources, Once, None),
        // -- Linking
        ("LINK_WHAT_YOU_USE", CanCompileSources, Once, None),
        // Build graph properties
        ("LINK_DEPENDS_NO_SHARED", CanCompileSources, Once, None),
        ("UNITY_BUILD", CanCompileSources, Once, None),
        ("UNITY_BUILD_UNIQUE_ID", CanCompileSources, Once, None),
        ("UNITY_BUILD_BATCH_SIZE", CanCompileSources, Once, Some("8")),
        ("UNITY_BUILD_MODE", CanCompileSources, Once, Some("BATCH")),
        ("UNITY_BUILD_RELOCATABLE", CanCompileSources, Once, None),
        ("OPTIMIZE_DEPENDENCIES", CanCompileSources, Once, None),
        ("VERIFY_INTERFACE_HEADER_SETS", Always, Once, None),
        // -- Android
        ("ANDROID_ANT_ADDITIONAL_OPTIONS", CanCompileSources, Once, None),
        ("ANDROID_PROCESS_MAX", CanCompileSources, Once, None),
        ("ANDROID_SKIP_ANT_STEP", CanCompileSources, Once, None),
        // -- Autogen
        ("AUTOGEN_COMMAND_LINE_LENGTH_MAX", CanCompileSources, Once, None),
        ("AUTOGEN_ORIGIN_DEPENDS", CanCompileSources, Once, None),
        ("AUTOGEN_PARALLEL", CanCompileSources, Once, None),
        ("AUTOGEN_USE_SYSTEM_INCLUDE", CanCompileSources, Once, None),
        ("AUTOGEN_BETTER_GRAPH_MULTI_CONFIG", CanCompileSources, Once, None),
        // -- moc
        ("AUTOMOC_DEPEND_FILTERS", CanCompileSources, Once, None),
        // -- C++
        ("CXX_SCAN_FOR_MODULES", CanCompileSources, Once, None),
        // -- Ninja
        ("JOB_POOL_COMPILE", CanCompileSources, Once, None),
        ("JOB_POOL_LINK", CanCompileSources, Once, None),
        ("JOB_POOL_PRECOMPILE_HEADER", CanCompileSources, Once, None),
        // -- Visual Studio
        ("VS_NO_COMPILE_BATCHING", CanCompileSources, Once, None),
        ("VS_WINDOWS_TARGET_PLATFORM_MIN_VERSION", CanCompileSources, Once, None),
        // Output location properties
        ("ARCHIVE_OUTPUT_DIRECTORY", CanCompileSources, Once, None),
        ("ARCHIVE_OUTPUT_DIRECTORY_", TargetWithArtifact, PerConfig, None),
        ("COMPILE_PDB_OUTPUT_DIRECTORY", CanCompileSources, Once, None),
        ("COMPILE_PDB_OUTPUT_DIRECTORY_", TargetWithArtifact, PerConfig, None),
        ("LIBRARY_OUTPUT_DIRECTORY", CanCompileSources, Once, None),
        ("LIBRARY_OUTPUT_DIRECTORY_", TargetWithArtifact, PerConfig, None),
        ("PDB_OUTPUT_DIRECTORY", CanCompileSources, Once, None),
        ("PDB_OUTPUT_DIRECTORY_", TargetWithArtifact, PerConfig, None),
        ("RUNTIME_OUTPUT_DIRECTORY", CanCompileSources, Once, None),
        ("RUNTIME_OUTPUT_DIRECTORY_", TargetWithArtifact, PerConfig, None),
        // macOS bundle properties
        ("FRAMEWORK", CanCompileSources, Once, None),
        ("FRAMEWORK_MULTI_CONFIG_POSTFIX", CanCompileSources, Once, None),
        ("MACOSX_BUNDLE", CanCompileSources, Once, None),
        // Usage requirement properties
        ("LINK_INTERFACE_LIBRARIES", CanCompileSources, Once, None),
        ("MAP_IMPORTED_CONFIG_", NormalTarget, PerConfig, None),
        ("EXPORT_FIND_PACKAGE_NAME", NormalTarget, Once, None),
        // Metadata
        ("CROSSCOMPILING_EMULATOR", ExecutableTarget, Once, None),
        ("EXPORT_BUILD_DATABASE", CanCompileSources, Once, None),
        ("EXPORT_COMPILE_COMMANDS", CanCompileSources, Once, None),
        ("FOLDER", Always, Once, None),
        ("TEST_LAUNCHER", ExecutableTarget, Once, None),
        // Xcode properties
        ("XCODE_GENERATE_SCHEME", NeedsXcode, Once, None),
        #[cfg(target_os = "macos")]
        ("XCODE_SCHEME_ADDRESS_SANITIZER", NeedsXcodeAndCanCompileSources, Once, None),
        #[cfg(target_os = "macos")]
        ("XCODE_SCHEME_ADDRESS_SANITIZER_USE_AFTER_RETURN", NeedsXcodeAndCanCompileSources, Once, None),
        #[cfg(target_os = "macos")]
        ("XCODE_SCHEME_DEBUG_DOCUMENT_VERSIONING", NeedsXcodeAndCanCompileSources, Once, None),
        #[cfg(target_os = "macos")]
        ("XCODE_SCHEME_ENABLE_GPU_FRAME_CAPTURE_MODE", NeedsXcodeAndCanCompileSources, Once, None),
        #[cfg(target_os = "macos")]
        ("XCODE_SCHEME_THREAD_SANITIZER", NeedsXcodeAndCanCompileSources, Once, None),
        #[cfg(target_os = "macos")]
        ("XCODE_SCHEME_THREAD_SANITIZER_STOP", NeedsXcodeAndCanCompileSources, Once, None),
        #[cfg(target_os = "macos")]
        ("XCODE_SCHEME_UNDEFINED_BEHAVIOUR_SANITIZER", NeedsXcodeAndCanCompileSources, Once, None),
        #[cfg(target_os = "macos")]
        ("XCODE_SCHEME_UNDEFINED_BEHAVIOUR_SANITIZER_STOP", NeedsXcodeAndCanCompileSources, Once, None),
        #[cfg(target_os = "macos")]
        ("XCODE_SCHEME_LAUNCH_CONFIGURATION", NeedsXcodeAndCanCompileSources, Once, None),
        #[cfg(target_os = "macos")]
        ("XCODE_SCHEME_TEST_CONFIGURATION", NeedsXcodeAndCanCompileSources, Once, None),
        #[cfg(target_os = "macos")]
        ("XCODE_SCHEME_ENABLE_GPU_API_VALIDATION", NeedsXcodeAndCanCompileSources, Once, None),
        #[cfg(target_os = "macos")]
        ("XCODE_SCHEME_ENABLE_GPU_SHADER_VALIDATION", NeedsXcodeAndCanCompileSources, Once, None),
        #[cfg(target_os = "macos")]
        ("XCODE_SCHEME_WORKING_DIRECTORY", NeedsXcodeAndCanCompileSources, Once, None),
        #[cfg(target_os = "macos")]
        ("XCODE_SCHEME_DISABLE_MAIN_THREAD_CHECKER", NeedsXcodeAndCanCompileSources, Once, None),
        #[cfg(target_os = "macos")]
        ("XCODE_SCHEME_MAIN_THREAD_CHECKER_STOP", NeedsXcodeAndCanCompileSources, Once, None),
        #[cfg(target_os = "macos")]
        ("XCODE_SCHEME_MALLOC_SCRIBBLE", NeedsXcodeAndCanCompileSources, Once, None),
        #[cfg(target_os = "macos")]
        ("XCODE_SCHEME_MALLOC_GUARD_EDGES", NeedsXcodeAndCanCompileSources, Once, None),
        #[cfg(target_os = "macos")]
        ("XCODE_SCHEME_GUARD_MALLOC", NeedsXcodeAndCanCompileSources, Once, None),
        #[cfg(target_os = "macos")]
        ("XCODE_SCHEME_LAUNCH_MODE", NeedsXcodeAndCanCompileSources, Once, None),
        #[cfg(target_os = "macos")]
        ("XCODE_SCHEME_LLDB_INIT_FILE", NeedsXcodeAndCanCompileSources, Once, None),
        #[cfg(target_os = "macos")]
        ("XCODE_SCHEME_ZOMBIE_OBJECTS", NeedsXcodeAndCanCompileSources, Once, None),
        #[cfg(target_os = "macos")]
        ("XCODE_SCHEME_MALLOC_STACK", NeedsXcodeAndCanCompileSources, Once, None),
        #[cfg(target_os = "macos")]
        ("XCODE_SCHEME_DYNAMIC_LINKER_API_USAGE", NeedsXcodeAndCanCompileSources, Once, None),
        #[cfg(target_os = "macos")]
        ("XCODE_SCHEME_DYNAMIC_LIBRARY_LOADS", NeedsXcodeAndCanCompileSources, Once, None),
        #[cfg(target_os = "macos")]
        ("XCODE_SCHEME_ENVIRONMENT", NeedsXcodeAndCanCompileSources, Once, None),
        #[cfg(target_os = "macos")]
        ("XCODE_LINK_BUILD_PHASE_MODE", NeedsXcodeAndCanCompileSources, Once, Some("NONE")),
    ]
};

pub const DIRECTORY_USAGE_PROPERTIES: &[&str] = &[
    "INCLUDE_DIRECTORIES",
    "COMPILE_OPTIONS",
    "COMPILE_DEFINITIONS",
    "LINK_OPTIONS",
    "LINK_DIRECTORIES",
    "LINK_LIBRARIES",
];

pub fn policy_warning(id: &str) -> String {
    policy::policy_warning(policy::index_of(id).expect("known policy"))
}

pub fn report<T>(interp: &mut Interp, msg: &str) -> Result<T, CmdErr> {
    interp.issue(MessageType::FatalError, msg);
    Err(CmdErr::Reported)
}

pub fn valid_target_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.:+-".contains(&byte))
}

pub fn check_target_name(interp: &mut Interp, name: &str, allow_colon: bool) -> CmdResult {
    const RESERVED: &[&str] = &[
        "all",
        "ALL_BUILD",
        "help",
        "install",
        "INSTALL",
        "preinstall",
        "clean",
        "edit_cache",
        "rebuild_cache",
        "ZERO_CHECK",
    ];
    if valid_target_name(name) && (allow_colon || !name.contains(':')) && !RESERVED.contains(&name)
    {
        return Ok(());
    }
    report(
        interp,
        &format!(
            "The target name \"{name}\" is reserved or not valid for certain CMake features, such as generator expressions, and may result in undefined behavior."
        ),
    )
}

pub fn enforce_unique_name(interp: &Interp, name: &str, custom: bool) -> CmdResult {
    if find_alias(interp, name).is_some() {
        return err(format!(
            "cannot create target \"{name}\" because an alias with the same name already exists."
        ));
    }
    let Some(existing) = find_target_to_use(interp, name) else {
        return Ok(());
    };
    if existing.imported {
        return err(format!(
            "cannot create target \"{name}\" because an imported target with the same name already exists."
        ));
    }
    let kind = match existing.kind {
        TargetKind::Utility
            if custom
                && interp.mf_index() != existing.makefile
                && interp
                    .global_props
                    .get("ALLOW_DUPLICATE_CUSTOM_TARGETS")
                    .is_some_and(is_on) =>
        {
            return Ok(());
        }
        TargetKind::Executable => "an executable ",
        TargetKind::StaticLibrary => "a static library ",
        TargetKind::SharedLibrary => "a shared library ",
        TargetKind::ModuleLibrary => "a module library ",
        TargetKind::Utility => "a custom target ",
        TargetKind::InterfaceLibrary => "an interface library ",
        _ => "",
    };
    let dir = &interp.makefiles[existing.makefile].source_dir;
    err(format!(
        "cannot create target \"{name}\" because another target with the same name already exists.  The existing target is {kind}created in source directory \"{dir}\".  See documentation for policy CMP0002 for more details."
    ))
}

pub fn create_target(
    interp: &mut Interp,
    name: &str,
    kind: TargetKind,
    imported: bool,
    global: bool,
) -> String {
    use TargetKind::*;
    let dir = interp.mf_index();
    let mut target = Target::new(name, kind, dir);
    target.imported = imported;
    target.imported_global = imported && global;
    target.backtrace.clone_from(&interp.backtrace);
    target
        .find_package_stack
        .clone_from(&interp.ext.find_package_stack);
    target.policies = interp.effective_policies();
    // TYPE, NAME, SOURCE_DIR, BINARY_DIR, IMPORTED[_GLOBAL] and the file
    // set lists are computed on read (cmTargetPropertyComputer), not stored.
    if !imported {
        for prop in [
            "INCLUDE_DIRECTORIES",
            "COMPILE_OPTIONS",
            "LINK_OPTIONS",
            "LINK_DIRECTORIES",
        ] {
            if let Some(value) = interp.mf().props.get(prop) {
                target.props.set(prop, value.to_owned());
                target
                    .provenance
                    .insert(prop.to_owned(), directory_entries(interp, dir, prop));
            }
        }
        if let Some(system) = interp.ext.directory_system_includes.get(&dir) {
            target.system_include_directories.clone_from(system);
        }
    }
    let generator = interp.get_def("CMAKE_GENERATOR").unwrap_or_default();
    let multiconfig = generator == "Ninja Multi-Config"
        || generator == "Xcode"
        || generator.starts_with("Visual Studio")
        || generator.starts_with("Green Hills");
    let config_var = if multiconfig {
        "CMAKE_CONFIGURATION_TYPES"
    } else {
        "CMAKE_BUILD_TYPE"
    };
    let config_value = interp.get_def(config_var).unwrap_or_default();
    let configs = if multiconfig {
        expand_list(config_value, false)
    } else if config_value.is_empty() {
        Vec::new()
    } else {
        vec![config_value.to_owned()]
    };
    let can_compile = !imported
        && matches!(
            kind,
            Executable | StaticLibrary | SharedLibrary | ModuleLibrary | ObjectLibrary
        );
    let normal = !matches!(kind, Utility | GlobalTarget);
    let artifact = normal && kind != InterfaceLibrary;
    for &(prop, condition, repeat, default) in INITIALIZERS {
        let apply = match condition {
            Condition::Always => true,
            Condition::CanCompileSources => can_compile,
            Condition::ExecutableTarget => kind == Executable,
            Condition::NonImportedTarget => !imported,
            Condition::SharedLibraryTarget => kind == SharedLibrary,
            Condition::TargetWithCommands => !matches!(kind, InterfaceLibrary | UnknownLibrary),
            Condition::TargetWithSymbolExports => matches!(kind, Executable | SharedLibrary),
            Condition::NormalNonImportedTarget => normal && !imported,
            Condition::NormalTarget => normal,
            Condition::TargetWithArtifact => artifact,
            Condition::NonExecutableWithArtifact => artifact && kind != Executable,
            Condition::LinkableLibraryTarget => matches!(kind, SharedLibrary | StaticLibrary),
            Condition::NeedsXcode => generator == "Xcode",
            #[cfg(target_os = "macos")]
            Condition::NeedsXcodeAndCanCompileSources => generator == "Xcode" && can_compile,
        };
        if !apply || (imported && prop == "AIX_SHARED_LIBRARY_ARCHIVE") {
            continue;
        }
        let initialize = |target: &mut Target, prop: &str| {
            let value = if prop == "ENABLE_EXPORTS" {
                let specific = if kind == Executable {
                    "EXECUTABLE"
                } else {
                    "SHARED_LIBRARY"
                };
                interp
                    .get_def(&format!("CMAKE_{specific}_ENABLE_EXPORTS"))
                    .or_else(|| {
                        if kind == Executable {
                            interp.get_def("CMAKE_ENABLE_EXPORTS")
                        } else {
                            None
                        }
                    })
            } else {
                interp.get_def(&format!("CMAKE_{prop}"))
            };
            if let Some(value) = value.or(default) {
                target.set_entry(prop, Some(value), &interp.backtrace);
            }
        };
        match repeat {
            Repeat::Once => initialize(&mut target, prop),
            Repeat::PerConfig | Repeat::PerConfigPrefix => {
                for config in &configs {
                    let config = config.to_ascii_uppercase();
                    let prop = match repeat {
                        Repeat::PerConfig => format!("{prop}{config}"),
                        _ => format!("{config}{prop}"),
                    };
                    initialize(&mut target, &prop);
                }
            }
        }
    }
    if matches!(kind, SharedLibrary | ModuleLibrary) {
        target
            .props
            .set("POSITION_INDEPENDENT_CODE", "True".to_owned());
    }
    if imported || interp.mf().props.get("SYSTEM").is_some_and(is_on) {
        target.props.set("SYSTEM", "ON".to_owned());
    }
    if generator.starts_with("Visual Studio") {
        for entry in expand_list(
            interp.get_def("CMAKE_VS_GLOBALS").unwrap_or_default(),
            false,
        ) {
            if let Some((name, value)) = entry.split_once('=') {
                let prop = format!("VS_GLOBAL_{name}");
                let value = interp.get_def(&format!("CMAKE_{prop}")).unwrap_or(value);
                target.props.set(&prop, value.to_owned());
            }
        }
    }
    for (prop, var) in &interp.ext.target_property_initializers {
        if let Some(value) = interp.get_def(var) {
            target.set_entry(prop, Some(value), &interp.backtrace);
        }
    }
    register_target(interp, target)
}

/// Stores a target in `interp.ext` and returns its key.  Global targets are
/// keyed by name.  Directory-scoped ones (non-global IMPORTED targets, and
/// duplicates allowed by ALLOW_DUPLICATE_CUSTOM_TARGETS) get the key
/// `[<dir>]<name>` and are reachable through `scoped_targets` from their
/// directory and the subdirectories created after them.
pub fn register_target(interp: &mut Interp, target: Target) -> String {
    let dir = target.makefile;
    let name = target.name.clone();
    let key =
        if target.imported && !target.imported_global || interp.ext.targets.contains_key(&name) {
            let key = format!("[{dir}]{name}");
            interp.ext.scoped_keys_by_name.entry(name.clone()).or_default().push(key.clone());
            interp.ext.scoped_targets.insert((dir, name), key.clone());
            key
        } else {
            name
        };
    if !target.imported {
        interp.makefiles[dir].targets.push(key.clone());
    }
    interp.ext.target_order.push(key.clone());
    interp.ext.targets.insert(key.clone(), target);
    key
}

pub fn directory_entries(interp: &Interp, dir: usize, prop: &str) -> Vec<PropertyEntry> {
    let Some(value) = interp.makefiles[dir].props.get(prop) else {
        return Vec::new();
    };
    if let Some(entries) = interp
        .ext
        .directory_provenance
        .get(&dir)
        .and_then(|props| props.get(prop))
    {
        if entries
            .iter()
            .map(|(value, _)| value.as_str())
            .collect::<Vec<_>>()
            .join(";")
            == value
        {
            return entries.clone();
        }
    }
    vec![(value.to_owned(), Vec::new())]
}

pub fn append_directory(interp: &mut Interp, prop: &str, value: &str, prepend: bool) {
    let dir = interp.mf_index();
    let mut entries = directory_entries(interp, dir, prop);
    let entry = (value.to_owned(), interp.backtrace.clone());
    if prepend {
        entries.insert(0, entry);
    } else {
        entries.push(entry);
    }
    let value = entries
        .iter()
        .map(|(value, _)| value.as_str())
        .collect::<Vec<_>>()
        .join(";");
    interp.makefiles[dir].props.set(prop, value);
    interp
        .ext
        .directory_provenance
        .entry(dir)
        .or_default()
        .insert(prop.to_owned(), entries);
}

const USAGE_PROPERTIES: &[&str] = &[
    "INCLUDE_DIRECTORIES",
    "COMPILE_OPTIONS",
    "COMPILE_FEATURES",
    "COMPILE_DEFINITIONS",
    "PRECOMPILE_HEADERS",
    "SOURCES",
    "LINK_OPTIONS",
    "LINK_DIRECTORIES",
    "LINK_LIBRARIES",
    "INTERFACE_LINK_LIBRARIES",
    "INTERFACE_LINK_LIBRARIES_DIRECT",
    "INTERFACE_LINK_LIBRARIES_DIRECT_EXCLUDE",
    "IMPORTED_CXX_MODULES_INCLUDE_DIRECTORIES",
    "IMPORTED_CXX_MODULES_COMPILE_DEFINITIONS",
    "IMPORTED_CXX_MODULES_COMPILE_FEATURES",
    "IMPORTED_CXX_MODULES_COMPILE_OPTIONS",
    "IMPORTED_CXX_MODULES_LINK_LIBRARIES",
];

impl Target {
    pub fn set_entry(&mut self, prop: &str, value: Option<&str>, backtrace: &[Frame]) {
        if let Some(value) = value {
            self.props.set(prop, value.to_owned());
            if USAGE_PROPERTIES.contains(&prop)
                || prop.starts_with("INTERFACE_")
                || prop.ends_with("_STANDARD")
            {
                self.provenance.insert(
                    prop.to_owned(),
                    vec![(value.to_owned(), backtrace.to_vec())],
                );
            }
        } else {
            self.props.remove(prop);
            self.provenance.remove(prop);
        }
    }

    pub fn insert_entry(&mut self, prop: &str, value: &str, backtrace: &[Frame], prepend: bool) {
        let old = self.props.get(prop);
        let entries = self.provenance.entry(prop.to_owned()).or_default();
        if entries
            .iter()
            .map(|(value, _)| value.as_str())
            .collect::<Vec<_>>()
            .join(";")
            != old.unwrap_or_default()
            || (entries.is_empty() && old.is_some())
        {
            *entries = old
                .map(|old| vec![(old.to_owned(), Vec::new())])
                .unwrap_or_default();
        }
        let entry = (value.to_owned(), backtrace.to_vec());
        if prepend {
            entries.insert(0, entry);
        } else {
            entries.push(entry);
        }
        self.props.set(
            prop,
            entries
                .iter()
                .map(|(value, _)| value.as_str())
                .collect::<Vec<_>>()
                .join(";"),
        );
    }

    pub fn add_dependency(&mut self, name: &str, backtrace: &[Frame]) {
        self.dependencies
            .entry(name.to_owned())
            .or_insert_with(|| backtrace.to_vec());
        let list = self
            .dependencies
            .keys()
            .cloned()
            .collect::<Vec<_>>()
            .join(";");
        self.props.set("MANUALLY_ADDED_DEPENDENCIES", list);
    }
}

fn computed_property<'a>(
    interp: &'a Interp,
    target: &'a Target,
    prop: &str,
) -> Option<Cow<'a, str>> {
    let mf = &interp.makefiles[target.makefile];
    Some(Cow::Borrowed(match prop {
        "NAME" => &target.name,
        "TYPE" => target.kind.type_name(),
        "IMPORTED" if target.imported => "TRUE",
        "IMPORTED_GLOBAL" if target.imported_global => "TRUE",
        "IMPORTED" | "IMPORTED_GLOBAL" => "FALSE",
        "SOURCE_DIR" => &mf.source_dir,
        "BINARY_DIR" => &mf.binary_dir,
        "HEADER_SETS"
        | "INTERFACE_HEADER_SETS"
        | "CXX_MODULE_SETS"
        | "INTERFACE_CXX_MODULE_SETS" => target.props.get(prop).unwrap_or_default(),
        _ => return None,
    }))
}

fn location_config(kind: TargetKind, prop: &str) -> Option<&str> {
    if !matches!(
        kind,
        TargetKind::Executable
            | TargetKind::StaticLibrary
            | TargetKind::SharedLibrary
            | TargetKind::ModuleLibrary
            | TargetKind::UnknownLibrary
    ) {
        return None;
    }
    if prop == "LOCATION" {
        Some("")
    } else if let Some(config) = prop.strip_prefix("LOCATION_") {
        Some(config)
    } else {
        prop.strip_suffix("_LOCATION")
            .filter(|config| *config != "IMPORTED" && !prop.starts_with("XCODE_ATTRIBUTE_"))
    }
}

fn imported_location<'a>(target: &'a Target, config: &str) -> Option<&'a str> {
    crate::generator::target::imported_artifact_path(target, config, crate::generator::target::ArtifactType::Runtime)
}

fn file_set_property(prop: &str) -> Option<(&str, &str, bool)> {
    for (kind, dirs_prop, files_prop) in [
        ("HEADERS", "HEADER_DIRS", "HEADER_SET"),
        ("CXX_MODULES", "CXX_MODULE_DIRS", "CXX_MODULE_SET"),
    ] {
        for (prefix, dirs) in [(dirs_prop, true), (files_prop, false)] {
            if prefix == prop {
                return Some((kind, kind, dirs));
            }
            if let Some(name) = prop
                .strip_prefix(prefix)
                .and_then(|suffix| suffix.strip_prefix('_'))
            {
                return Some((kind, name, dirs));
            }
        }
    }
    None
}

/// Get the computed or stored configure-time target property. Alias metadata is
/// looked up by the spelling used by the caller.
#[allow(dead_code, reason = "used by the property command integration")]
pub fn target_property<'a>(interp: &'a Interp, name: &str, prop: &str) -> Option<Cow<'a, str>> {
    let target = find_target_to_use(interp, name)?;
    match prop {
        "ALIASED_TARGET" => {
            return find_alias(interp, name).map(|_| Cow::Borrowed(target.name.as_str()));
        }
        "ALIAS_GLOBAL" => {
            return find_alias(interp, name)
                .map(|(_, global)| Cow::Borrowed(if global { "TRUE" } else { "FALSE" }));
        }
        "SOURCES" => {
            return target.props.get(prop).map(|sources| {
                let resolved = expand_list(sources, false)
                    .into_iter()
                    .map(|source| {
                        if has_genex(&source)
                            && !(source.starts_with("$<TARGET_OBJECTS:") && source.ends_with('>'))
                        {
                            find_source(interp, target.makefile, &source).map_or_else(
                                || SourceFileLocation::new(interp, target.makefile, &source).path(),
                                |file| file.location.path(),
                            )
                        } else {
                            source
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(";");
                Cow::Owned(resolved)
            });
        }
        _ => {}
    }
    if let Some(config) = location_config(target.kind, prop) {
        return target.imported.then(|| {
            imported_location(target, config).map_or_else(
                || Cow::Owned(format!("{}-NOTFOUND", target.name)),
                Cow::Borrowed,
            )
        });
    }
    computed_property(interp, target, prop).or_else(|| target.props.get(prop).map(Cow::Borrowed))
}

/// Retrieve a property whose computation may issue a diagnostic.
pub fn read_target_property(interp: &mut Interp, name: &str, prop: &str) -> Option<String> {
    let target = find_target_to_use(interp, name)?;
    if let Some(config) = location_config(target.kind, prop) {
        if !target.imported {
            let name = target.name.clone();
            interp.issue(MessageType::FatalError, &format!(
                "The LOCATION property may not be read from target \"{name}\".  Use the target name directly with add_custom_command, or use the generator expression $<TARGET_FILE>, as appropriate.\n"
            ));
            return None;
        }
        if imported_location(target, config).is_none() {
            let status = target
                .policies
                .get(policy::index_of("CMP0111").unwrap())
                .unwrap_or(PolicyStatus::Warn);
            let property = if target.kind == TargetKind::SharedLibrary {
                "IMPORTED_LOCATION or IMPORTED_IMPLIB"
            } else {
                "IMPORTED_LOCATION"
            };
            let suffix = if config.is_empty() {
                String::new()
            } else {
                format!(" configuration \"{config}\"")
            };
            let msg = format!(
                "{property} not set for imported target \"{}\"{suffix}.",
                target.name
            );
            match status {
                PolicyStatus::New => interp.issue(MessageType::FatalError, &msg),
                PolicyStatus::Warn => interp.issue(
                    MessageType::AuthorWarning,
                    &format!("{}\n{msg}", policy_warning("CMP0111")),
                ),
                PolicyStatus::Old => {}
            }
        }
    } else if let Some((kind, file_set, _)) = file_set_property(prop) {
        if target
            .file_sets
            .get(file_set)
            .is_some_and(|set| set.kind != kind)
        {
            interp.issue(
                MessageType::FatalError,
                &format!("File set \"{file_set}\" is not of type \"{kind}\"."),
            );
            return None;
        }
    }
    target_property(interp, name, prop).map(Cow::into_owned)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PropertyWrite {
    Set,
    Append,
    AppendString,
}

/// Mutate target properties from set_property/set_target_properties.
pub fn write_target_property(
    interp: &mut Interp,
    name: &str,
    prop: &str,
    value: Option<&str>,
    how: PropertyWrite,
) -> CmdResult {
    if find_alias(interp, name).is_some() {
        return err("can not be used on an ALIAS target.");
    }
    let Some(key) = find_target_key(interp, name) else {
        return Ok(());
    };
    let target = &interp.ext.targets[&key];
    let owner = target.makefile;
    let read_only = match prop {
        "HEADER_SETS"
        | "INTERFACE_HEADER_SETS"
        | "MANUALLY_ADDED_DEPENDENCIES"
        | "NAME"
        | "TYPE" => Some(("is read-only for target", false)),
        "EXPORT_NAME" | "SOURCES" if target.imported => {
            Some(("can't be set on imported targets", false))
        }
        "IMPORTED_GLOBAL" if !target.imported => {
            Some(("can't be set on non-imported targets", false))
        }
        "ALIAS_GLOBAL"
        | "BINARY_DIR"
        | "CXX_MODULE_SETS"
        | "IMPORTED"
        | "INTERFACE_CXX_MODULE_SETS"
        | "LOCATION"
        | "LOCATION_CONFIG"
        | "SOURCE_DIR" => Some(("is read-only for target", true)),
        _ => None,
    };
    if let Some((reason, has_policy)) = read_only {
        let status = if has_policy {
            target
                .policies
                .get(policy::index_of("CMP0160").unwrap())
                .unwrap_or(PolicyStatus::Warn)
        } else {
            PolicyStatus::New
        };
        let msg = format!("{prop} property {reason}(\"{name}\")\n");
        match status {
            PolicyStatus::New => return report(interp, &msg),
            PolicyStatus::Warn => interp.issue(
                MessageType::AuthorWarning,
                &format!("{}\n{msg}", policy_warning("CMP0160")),
            ),
            PolicyStatus::Old => {}
        }
    }
    if prop == "IMPORTED_GLOBAL" {
        if how != PropertyWrite::Set {
            return report(
                interp,
                &format!(
                    "IMPORTED_GLOBAL property can't be appended, only set on imported targets (\"{name}\")\n"
                ),
            );
        }
        if !value.is_some_and(is_on) {
            return report(
                interp,
                &format!(
                    "IMPORTED_GLOBAL property can't be set to FALSE on targets (\"{name}\")\n"
                ),
            );
        }
        if owner != interp.mf_index() {
            return report(
                interp,
                &format!(
                    "Attempt to promote imported target \"{name}\" to global scope (by setting IMPORTED_GLOBAL) which is not built in this directory."
                ),
            );
        }
        let target = interp.ext.targets.get_mut(&key).unwrap();
        target.imported_global = true;
        target.props.set(prop, "TRUE".to_owned());
        return Ok(());
    }
    if let Some((kind, name, dirs)) = file_set_property(prop) {
        let title = if kind == "HEADERS" {
            "Header"
        } else {
            "C++ module"
        };
        if name.is_empty() {
            return report(interp, &format!("{title} set name cannot be empty."));
        }
        let target = interp.ext.targets.get_mut(&key).unwrap();
        let Some(set) = target.file_sets.get_mut(name) else {
            let description = if kind == name {
                format!("The default {title} set").replace("default Header", "default header")
            } else {
                format!("{title} set \"{name}\"")
            };
            return report(interp, &format!("{description}has not yet been created."));
        };
        if set.kind != kind {
            return report(
                interp,
                &format!("File set \"{name}\" is not of type \"{kind}\"."),
            );
        }
        let entries = if dirs {
            &mut set.base_dirs
        } else {
            &mut set.files
        };
        if how == PropertyWrite::Set {
            entries.clear();
        }
        if let Some(value) = value.filter(|value| !value.is_empty()) {
            entries.push((value.to_owned(), interp.backtrace.clone()));
        }
        let value = entries
            .iter()
            .map(|(value, _)| value.as_str())
            .collect::<Vec<_>>()
            .join(";");
        target.props.set(prop, value);
        return Ok(());
    }
    let target = &interp.ext.targets[&key];
    if prop.starts_with("IMPORTED_LIBNAME") && how == PropertyWrite::Set {
        if target.kind != TargetKind::InterfaceLibrary || !target.imported {
            return report(
                interp,
                &format!("{prop} property may be set only on imported INTERFACE library targets."),
            );
        }
        if let Some(value) = value {
            if value.starts_with('-') {
                return report(
                    interp,
                    &format!("{prop} property value\n  {value}\nmay not start with '-'."),
                );
            }
            if let Some(bad) = value.chars().find(|ch| [':', '/', '\\', ';'].contains(ch)) {
                return report(
                    interp,
                    &format!("{prop} property value\n  {value}\nmay not contain '{bad}'."),
                );
            }
        }
    }
    if how == PropertyWrite::Set {
        if let Some(flag) = match prop {
            "CUDA_PTX_COMPILATION" => Some("_CMAKE_CUDA_PTX_FLAG"),
            "CUDA_CUBIN_COMPILATION" => Some("_CMAKE_CUDA_CUBIN_FLAG"),
            "CUDA_FATBIN_COMPILATION" => Some("_CMAKE_CUDA_FATBIN_FLAG"),
            "CUDA_OPTIX_COMPILATION" => Some("_CMAKE_CUDA_OPTIX_FLAG"),
            _ => None,
        } {
            if target.kind != TargetKind::ObjectLibrary {
                return report(
                    interp,
                    &format!("{prop} property can only be applied to OBJECT targets({name})\n"),
                );
            }
            if !interp.is_def_set(flag) {
                let compiler = interp.get_safe("CMAKE_CUDA_COMPILER_ID");
                let version = interp.get_safe("CMAKE_CUDA_COMPILER_VERSION");
                return report(
                    interp,
                    &format!(
                        "{prop} property is not supported by {compiler}  compiler version {version}."
                    ),
                );
            }
        }
    }
    if prop == "PRECOMPILE_HEADERS_REUSE_FROM" && how == PropertyWrite::Set {
        if target.props.contains("PRECOMPILE_HEADERS") {
            return report(
                interp,
                &format!("PRECOMPILE_HEADERS property is already set on target (\"{name}\")\n"),
            );
        }
        let Some(reused_key) = value.and_then(|value| {
            interp
                .ext
                .aliases
                .get(value)
                .cloned()
                .or_else(|| find_global_target_key(interp, value))
        }) else {
            return report(
                interp,
                "PRECOMPILE_HEADERS_REUSE_FROM set with non existing target",
            );
        };
        let reused = interp.ext.targets.get_mut(&reused_key).unwrap();
        let origin = reused
            .props
            .get(prop)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| value.unwrap())
            .to_owned();
        reused.props.set("COMPILE_PDB_NAME", origin.clone());
        reused
            .props
            .set("COMPILE_PDB_OUTPUT_DIRECTORY", format!("{origin}.dir/"));
        let target = interp.ext.targets.get_mut(&key).unwrap();
        target.props.set(prop, origin.clone());
        target.props.set("COMPILE_PDB_NAME", origin.clone());
        target.add_dependency(&origin, &interp.backtrace);
        return Ok(());
    }
    if prop == "PRECOMPILE_HEADERS"
        && how != PropertyWrite::Set
        && target.props.contains("PRECOMPILE_HEADERS_REUSE_FROM")
    {
        return report(
            interp,
            &format!(
                "PRECOMPILE_HEADERS_REUSE_FROM property is already set on target (\"{name}\")\n"
            ),
        );
    }
    let target = interp.ext.targets.get_mut(&key).unwrap();
    if how != PropertyWrite::Set && prop != "SOURCES" && value.is_none_or(str::is_empty) {
        return Ok(());
    }
    match how {
        PropertyWrite::Set => target.set_entry(prop, value, &interp.backtrace),
        _ if USAGE_PROPERTIES.contains(&prop)
            || (prop.starts_with("INTERFACE_") && how == PropertyWrite::Append) =>
        {
            target.insert_entry(prop, value.unwrap_or_default(), &interp.backtrace, false)
        }
        _ => target.props.append(
            prop,
            value.unwrap_or_default(),
            how == PropertyWrite::AppendString,
        ),
    }
    let legacy_interface = prop.starts_with("LINK_INTERFACE_LIBRARIES");
    let imported_interface = prop.starts_with("IMPORTED_LINK_INTERFACE_LIBRARIES");
    if prop == "INTERFACE_LINK_LIBRARIES" || legacy_interface || imported_interface {
        let keyword = target.props.get(prop).and_then(|value| {
            value
                .split(';')
                .find(|value| matches!(*value, "debug" | "optimized" | "general"))
        });
        if let Some(keyword) = keyword {
            let message = if prop == "INTERFACE_LINK_LIBRARIES" {
                format!(
                    "Property {prop} may not contain link-type keyword \"{keyword}\".  The INTERFACE_LINK_LIBRARIES property may contain configuration-sensitive generator-expressions which may be used to specify per-configuration rules."
                )
            } else {
                let base = if legacy_interface {
                    "LINK_INTERFACE_LIBRARIES"
                } else {
                    "IMPORTED_LINK_INTERFACE_LIBRARIES"
                };
                let mut msg = format!(
                    "Property {prop} may not contain link-type keyword \"{keyword}\".  The {base} property has a per-configuration version called {base}_<CONFIG> which may be used to specify per-configuration rules."
                );
                if legacy_interface {
                    msg.push_str("  Alternatively, an IMPORTED library may be created, configured with a per-configuration location, and then named in the property value.  See the add_library command's IMPORTED mode for details.\nIf you have a list of libraries that already contains the keyword, use the target_link_libraries command with its LINK_INTERFACE_LIBRARIES mode to set the property.  The command automatically recognizes link-type keywords and sets the LINK_INTERFACE_LIBRARIES and LINK_INTERFACE_LIBRARIES_DEBUG properties accordingly.");
                }
                msg
            };
            return report(interp, &message);
        }
    }
    Ok(())
}

const SOURCE_EXTENSIONS: &[&str] = &[
    "c", "C", "c++", "cc", "cpp", "cxx", "cu", "mpp", "m", "M", "mm", "ixx", "cppm", "ccm", "cxxm",
    "c++m", "h", "hh", "h++", "hm", "hpp", "hxx", "in", "txx", "f", "F", "for", "f77", "f90",
    "f95", "f03", "hip", "ispc",
];

pub fn genex_start(path: &str) -> bool {
    path.starts_with("$<") && path.contains('>')
}

pub fn has_genex(path: &str) -> bool {
    path.find("$<")
        .is_some_and(|pos| path[pos + 2..].contains('>'))
}

pub fn full_path(path: &str) -> bool {
    path.starts_with(['/', '~']) || (cfg!(windows) && Path::new(path).is_absolute())
}

impl SourceFileLocation {
    fn new(interp: &Interp, dir: usize, path: &str) -> Self {
        let (parent, name) = path.rsplit_once('/').map_or(("", path), |(parent, name)| {
            (if parent.is_empty() { "/" } else { parent }, name)
        });
        let mut location = Self {
            directory: if full_path(parent) {
                collapse_full_path(parent, "")
            } else {
                parent.to_owned()
            },
            name: name.to_owned(),
            ambiguous_directory: !full_path(path),
            ambiguous_extension: !(interp.policy_status_at(dir, "CMP0115") == PolicyStatus::New
                && interp.policy_status_at(dir, "CMP0187") == PolicyStatus::New),
        };
        let ext = name.rsplit_once('.').map_or("", |(_, ext)| ext);
        let known_extension = SOURCE_EXTENSIONS.contains(&ext)
            || interp.langs.enabled.iter().any(|lang| {
                expand_list(
                    interp
                        .get_def(&format!("CMAKE_{lang}_SOURCE_FILE_EXTENSIONS"))
                        .unwrap_or_default(),
                    false,
                )
                .iter()
                .any(|candidate| candidate == ext)
            });
        if known_extension {
            location.ambiguous_extension = false;
        } else {
            let source = &interp.makefiles[dir].source_dir;
            let absolute = collapse_full_path(&location.path(), source);
            if Path::new(&absolute).is_file() {
                location.directory = collapse_full_path(parent, source);
                location.ambiguous_directory = false;
                location.ambiguous_extension = false;
            }
        }
        location
    }

    pub fn path(&self) -> String {
        if self.directory.is_empty() {
            self.name.clone()
        } else {
            format!("{}/{}", self.directory.trim_end_matches('/'), self.name)
        }
    }

    fn matches(&self, other: &Self, source: &str, binary: &str) -> bool {
        let names_match = if self.name == other.name {
            true
        } else if self.ambiguous_extension != other.ambiguous_extension {
            let (short, long) = if self.ambiguous_extension {
                (&self.name, &other.name)
            } else {
                (&other.name, &self.name)
            };
            long.strip_prefix(short)
                .and_then(|rest| rest.strip_prefix('.'))
                .is_some_and(|ext| SOURCE_EXTENSIONS.contains(&ext))
        } else {
            false
        };
        if !names_match {
            return false;
        }
        if self.ambiguous_directory == other.ambiguous_directory {
            return self.directory == other.directory;
        }
        let (relative, absolute) = if self.ambiguous_directory {
            (&self.directory, &other.directory)
        } else {
            (&other.directory, &self.directory)
        };
        collapse_full_path(relative, source) == *absolute
            || collapse_full_path(relative, binary) == *absolute
    }
}

pub fn get_or_create_source<'a>(
    interp: &'a mut Interp,
    dir_index: usize,
    path: &str,
) -> &'a mut SourceFile {
    let index = source_index(interp, dir_index, path, false);
    &mut interp.ext.sources.get_mut(&dir_index).unwrap()[index]
}

/// The lookup key of a source name: `cmSourceFileLocation::Matches` only
/// pairs names that are equal or differ by a known source extension, so the
/// name without a known extension groups every possible match.
fn source_key(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((stem, ext)) if SOURCE_EXTENSIONS.contains(&ext) => stem,
        _ => name,
    }
}

/// The first source of directory `dir` matching `location`, by insertion
/// order (`cmMakefile::GetSource` iterates the name-indexed candidates).
fn matching_source(interp: &Interp, dir: usize, location: &SourceFileLocation) -> Option<usize> {
    let sources = interp.ext.sources.get(&dir)?;
    let mf = &interp.makefiles[dir];
    let matches = |index: &usize| {
        sources[*index]
            .location
            .matches(location, &mf.source_dir, &mf.binary_dir)
    };
    match interp.ext.source_name_index.get(&(dir, source_key(&location.name).to_owned())) {
        Some(candidates) => candidates.iter().copied().find(|index| matches(index)),
        // Sources inserted directly (test fixtures) are not indexed.
        None if interp.ext.source_name_index.is_empty() => (0..sources.len()).find(matches),
        None => None,
    }
}

/// `cmMakefile::GetOrCreateSource` returning the index into
/// `ext.sources[dir_index]`.  `known` is `cmSourceFileLocationKind::Known`
/// (the name's extension is not ambiguous).
pub fn source_index(interp: &mut Interp, dir_index: usize, path: &str, known: bool) -> usize {
    let mut location = SourceFileLocation::new(interp, dir_index, path);
    if known {
        location.ambiguous_extension = false;
    }
    if let Some(index) = matching_source(interp, dir_index, &location) {
        let source = &mut interp.ext.sources.get_mut(&dir_index).unwrap()[index];
        if source.location.ambiguous_directory && !location.ambiguous_directory {
            source.location.directory = location.directory;
            source.location.ambiguous_directory = false;
        }
        if source.location.ambiguous_extension && !location.ambiguous_extension {
            source.location.name = location.name;
            source.location.ambiguous_extension = false;
        }
        return index;
    }
    let key = (dir_index, source_key(&location.name).to_owned());
    let sources = interp.ext.sources.entry(dir_index).or_default();
    sources.push(SourceFile {
        makefile: dir_index,
        props: Default::default(),
        location,
        generated: false,
        custom_command: None,
    });
    let index = sources.len() - 1;
    interp.ext.source_name_index.entry(key).or_default().push(index);
    index
}

/// `cmMakefile::GetSource` with `cmSourceFileLocationKind::Known`.
pub fn find_known_source(interp: &Interp, dir: usize, path: &str) -> Option<usize> {
    let mut location = SourceFileLocation::new(interp, dir, path);
    location.ambiguous_extension = false;
    matching_source(interp, dir, &location)
}

/// `cmMakefile::GetSource` (ambiguous location) returning the index.
pub fn find_source_index(interp: &Interp, dir: usize, path: &str) -> Option<usize> {
    let location = SourceFileLocation::new(interp, dir, path);
    matching_source(interp, dir, &location)
}

pub fn find_source<'a>(interp: &'a Interp, dir: usize, path: &str) -> Option<&'a SourceFile> {
    let index = find_source_index(interp, dir, path)?;
    Some(&interp.ext.sources[&dir][index])
}

/// A source file's stored property (`cmSourceFile::GetProperty` for plain
/// properties), looked up with `cmSourceFileLocation` matching in directory
/// `dir_index`.  `None` when the source is unknown or the property is unset.
#[allow(dead_code, reason = "used by the source property commands")]
pub fn source_prop<'a>(
    interp: &'a Interp,
    dir_index: usize,
    path: &str,
    name: &str,
) -> Option<&'a str> {
    find_source(interp, dir_index, path)?.props.get(name)
}

pub fn mark_generated(interp: &mut Interp, dir: usize, path: &str) {
    if has_genex(path) {
        return;
    }
    let path = collapse_full_path(path, &interp.makefiles[dir].binary_dir);
    interp.ext.generated_sources.insert(path.clone());
    let source = get_or_create_source(interp, dir, &path);
    source.generated = true;
    source.props.set("GENERATED", "1".to_owned());
}

#[allow(dead_code, reason = "source generation API")]
pub struct ResolvedSource {
    pub path: String,
    pub language: Option<String>,
    pub generated: bool,
    pub external_object: bool,
    pub extension_warning: Option<String>,
}

#[allow(dead_code, reason = "source generation API")]
pub fn resolve_source(interp: &Interp, dir: usize, path: &str) -> Result<ResolvedSource, String> {
    let source = find_source(interp, dir, path);
    let fallback = SourceFileLocation::new(interp, dir, path);
    let location = source.map_or(&fallback, |source| &source.location);
    let mf = &interp.makefiles[dir];
    let path = location.path();
    let cmp0115 = interp.policy_status_at(dir, "CMP0115");
    let guess = cmp0115 != PolicyStatus::New;
    let global = interp.policy_status_at(dir, "CMP0118") == PolicyStatus::New
        || interp.policy_status_at(dir, "CMP0163") == PolicyStatus::New;
    let local_generated = source
        .is_some_and(|source| source.generated || source.props.get("GENERATED").is_some_and(is_on));
    let resolved = |path: String, generated, guessed| {
        let ext = path.rsplit_once('.').map_or("", |(_, ext)| ext);
        let language = source
            .and_then(|source| source.props.get("LANGUAGE"))
            .map(str::to_owned)
            .or_else(|| {
                interp
                    .langs
                    .enabled
                    .iter()
                    .find(|lang| {
                        expand_list(
                            interp
                                .get_def(&format!("CMAKE_{lang}_SOURCE_FILE_EXTENSIONS"))
                                .unwrap_or_default(),
                            false,
                        )
                        .iter()
                        .any(|candidate| candidate == ext)
                    })
                    .cloned()
            });
        let extension_warning = (guessed && cmp0115 == PolicyStatus::Warn)
            .then(|| format!("{}\nFile:\n  {path}", policy_warning("CMP0115")));
        let external_object = matches!(ext, "o" | "obj" | "lo")
            || source.is_some_and(|source| source.props.get("EXTERNAL_OBJECT").is_some_and(is_on));
        ResolvedSource {
            external_object,
            path,
            language,
            generated,
            extension_warning,
        }
    };
    if local_generated {
        return Ok(resolved(
            collapse_full_path(&path, &mf.binary_dir),
            true,
            false,
        ));
    }
    for base in [&mf.source_dir, &mf.binary_dir] {
        let candidate = collapse_full_path(&path, base);
        let generated = global && interp.ext.generated_sources.contains(&candidate);
        if generated || Path::new(&candidate).exists() {
            return Ok(resolved(candidate, generated, false));
        }
        if guess {
            for ext in SOURCE_EXTENSIONS {
                let candidate = format!("{candidate}.{ext}");
                let generated = global && interp.ext.generated_sources.contains(&candidate);
                if generated || Path::new(&candidate).exists() {
                    return Ok(resolved(candidate, generated, true));
                }
            }
        }
        if !location.ambiguous_directory {
            break;
        }
    }
    let mut message = format!("Cannot find source file:\n  {path}");
    if guess {
        message.push_str(&format!(
            "\nTried extensions .{}",
            SOURCE_EXTENSIONS.join(" .")
        ));
    }
    if path == "FILE_SET" {
        message.push_str("\nHint: the FILE_SET keyword may only appear after a visibility specifier or another FILE_SET within the target_sources() command.");
    }
    Err(message)
}

pub fn add_sources(interp: &mut Interp, key: &str, sources: &[String]) {
    if sources.is_empty() {
        return;
    }
    let dir = interp.ext.targets[key].makefile;
    for path in sources.iter().filter(|path| !path.starts_with("$<")) {
        get_or_create_source(interp, dir, path);
    }
    interp.ext.targets.get_mut(key).unwrap().insert_entry(
        "SOURCES",
        &sources.join(";"),
        &interp.backtrace,
        false,
    );
}

pub fn create_alias(interp: &mut Interp, alias: &str, name: &str, executable: bool) -> CmdResult {
    let prefix = format!("cannot create ALIAS target \"{alias}\" because target \"{name}\"");
    if find_alias(interp, name).is_some() {
        return err(format!("{prefix} is itself an ALIAS."));
    }
    let Some(key) = find_target_key(interp, name) else {
        return err(format!("{prefix} does not already exist."));
    };
    let target = &interp.ext.targets[&key];
    match target.kind {
        TargetKind::Executable if executable => {}
        TargetKind::Executable | TargetKind::Utility | TargetKind::GlobalTarget if !executable => {
            return err(format!("{prefix} is not a library."));
        }
        _ if executable => return err(format!("{prefix} is not an executable.")),
        _ => {}
    }
    if !target.imported || target.imported_global {
        interp.ext.aliases.insert(alias.to_owned(), key);
    } else {
        interp
            .ext
            .scoped_aliases
            .insert((interp.mf_index(), alias.to_owned()), key);
    }
    Ok(())
}

pub fn format_signature_traces(interp: &Interp, target: &Target, keyword: bool) -> String {
    let signature = if keyword { "keyword" } else { "plain" };
    let mut result = format!("The uses of the {signature} signature are here:\n");
    for (_, trace) in target
        .link_signatures
        .iter()
        .filter(|(found, _)| *found == keyword)
    {
        let file = relative_if_under(&interp.home_source, &trace.file);
        result.push_str(&format!(
            " * {file}:{} ({})\n",
            trace.line,
            trace.name.as_deref().unwrap_or_default()
        ));
    }
    result
}
