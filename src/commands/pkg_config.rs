//! Port of `Source/cmCMakePkgConfigCommand.cxx` — the `cmake_pkg_config`
//! command with EXTRACT, POPULATE and IMPORT subcommands.

use std::collections::HashMap;
use std::ffi::CString;
use std::io::Read;
use std::path::Path;

use crate::cache::CacheType;
use crate::interp::{CmdResult, Interp, err};
use crate::messages::MessageType;
use crate::pkgconfig::parser::{PCE_OK, Parser};
use crate::pkgconfig::resolver::{
    self, Env, PkgConfig, VersionReq, check_version, parse_version,
};
use crate::state::{TargetKind, find_target_to_use};
use crate::util::{
    collapse_full_path, expand_list, is_notfound, is_on_opt, join_list,
};

pub fn register(interp: &mut Interp) {
    interp.register("cmake_pkg_config", cmake_pkg_config);
}

// ---------------------------------------------------------------------------
// System-tool equivalents (cmSystemTools / kwsys)

/// `cmSystemTools::GetEnv`.
fn get_env(name: &str) -> Option<String> {
    std::env::var_os(name).map(|v| v.to_string_lossy().into_owned())
}

/// `cmSystemTools::HasEnv`.
fn has_env(name: &str) -> bool {
    std::env::var_os(name).is_some()
}

fn cstring(text: &str) -> Option<CString> {
    CString::new(text).ok()
}

/// `cmSystemTools::FileExists` (POSIX `access(path, R_OK)`).
fn file_exists(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    match cstring(path) {
        Some(c) => unsafe { libc::access(c.as_ptr(), libc::R_OK) == 0 },
        None => false,
    }
}

/// `cmSystemTools::FileIsDirectory` (stat + S_ISDIR).
fn file_is_directory(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    Path::new(path).is_dir()
}

/// `cmSystemTools::FileIsExecutable`.
fn file_is_executable(path: &str) -> bool {
    if file_is_directory(path) {
        return false;
    }
    match cstring(path) {
        Some(c) => unsafe { libc::access(c.as_ptr(), libc::X_OK) == 0 },
        None => false,
    }
}

/// `SystemToolsStatic::FindName`: first `dir/name` that `FileExists`.
fn find_name(name: &str, user_paths: &[String]) -> String {
    for p in user_paths {
        let mut try_path = p.clone();
        if try_path.is_empty() || !try_path.ends_with('/') {
            try_path.push('/');
        }
        try_path.push_str(name);
        if file_exists(&try_path) {
            return try_path;
        }
    }
    String::new()
}

/// `cmSystemTools::FindFile(name, paths, no_system_path = true)`.
fn find_file(name: &str, user_paths: &[String]) -> String {
    let try_path = find_name(name, user_paths);
    if !try_path.is_empty() && !file_is_directory(&try_path) {
        return collapse_full_path(&try_path, "");
    }
    String::new()
}

/// `SystemTools::GetPath` for PATH splitting (':' separators, empty
/// elements preserved, trailing separator synthesized).
fn path_env_entries() -> Vec<String> {
    let Some(mut value) = get_env("PATH") else {
        return Vec::new();
    };
    if value.is_empty() {
        return Vec::new();
    }
    if !value.ends_with(':') {
        value.push(':');
    }
    value.pop();
    value.split(':').map(str::to_owned).collect()
}

/// `cmSystemTools::FindProgram(name)` (POSIX behavior; no .exe/.com probes).
fn find_program(name: &str) -> String {
    if file_is_executable(name) {
        return collapse_full_path(name, "");
    }
    for mut p in path_env_entries() {
        if p.is_empty() || !p.ends_with('/') {
            p.push('/');
        }
        let try_path = format!("{p}{name}");
        if file_is_executable(&try_path) {
            return collapse_full_path(&try_path, "");
        }
    }
    String::new()
}

/// `cmSystemTools::RunSingleCommand` capturing stdout only.
fn run_single_command(command: &[String]) -> String {
    let Some(program) = command.first() else {
        return String::new();
    };
    match std::process::Command::new(program)
        .args(&command[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
    {
        Ok(output) => String::from_utf8_lossy(&output.stdout).into_owned(),
        Err(_) => String::new(),
    }
}

/// `cmSystemTools::SplitString(text, sep)` with `isPath = false`: keeps empty
/// elements, but a separator immediately following another is kept as data
/// (the scan resumes one position *after* the next element start).
fn split_string(path: &str, sep: char) -> Vec<String> {
    let bytes = path.as_bytes();
    let sep = sep as u8;
    let mut paths = Vec::new();
    if bytes.is_empty() {
        return paths;
    }
    let find = |from: usize| -> Option<usize> {
        bytes
            .iter()
            .enumerate()
            .skip(from)
            .find(|&(_, &b)| b == sep)
            .map(|(i, _)| i)
    };
    let mut pos1 = 0usize;
    let mut pos2 = find(0);
    while let Some(p2) = pos2 {
        paths.push(path[pos1..p2].to_owned());
        pos1 = p2 + 1;
        pos2 = find(pos1 + 1);
    }
    paths.push(path[pos1..].to_owned());
    paths
}

// ---------------------------------------------------------------------------
// Cache-variable / environment collection

/// `GetPkgConfigBin`.
fn get_pkg_config_bin(interp: &mut Interp) -> Option<String> {
    if let Some(pkgcfg) = interp.get_def("CMAKE_PKG_CONFIG_BIN").map(str::to_owned) {
        if is_notfound(&pkgcfg) {
            return None;
        }
        return Some(pkgcfg);
    }

    let mut path = find_program("pkgconf");
    if path.is_empty() {
        path = find_program("pkg-config");
        if path.is_empty() {
            interp.add_cache_def(
                "CMAKE_PKG_CONFIG_BIN",
                Some("pkg-config-NOTFOUND"),
                Some("Location of pkg-config or pkgconf binary"),
                CacheType::Filepath,
                false,
            );
            return None;
        }
    }

    interp.add_cache_def(
        "CMAKE_PKG_CONFIG_BIN",
        Some(&path),
        Some("Location of pkg-config or pkgconf binary"),
        CacheType::Filepath,
        false,
    );
    Some(path)
}

/// `GetLocations`.
fn get_locations(
    interp: &mut Interp,
    cachevar: &str,
    envvar: &str,
    desc: &str,
    pcvar: &str,
    need_pkgconf: bool,
    default_locs: Vec<String>,
) -> Vec<String> {
    if let Some(def) = interp.get_def(cachevar).map(str::to_owned) {
        return expand_list(&def, false);
    }

    if let Some(mut paths) = get_env(envvar) {
        resolver::replace_sep(&mut paths);
        interp.add_cache_def(cachevar, Some(&paths), Some(desc), CacheType::String, false);
        return expand_list(&paths, false);
    }

    let pkgcfg = get_pkg_config_bin(interp);
    let Some(bin) = pkgcfg else {
        let joined = join_list(&default_locs);
        interp.add_cache_def(cachevar, Some(&joined), Some(desc), CacheType::String, false);
        return default_locs;
    };
    if need_pkgconf && !bin.contains("pkgconf") {
        let joined = join_list(&default_locs);
        interp.add_cache_def(cachevar, Some(&joined), Some(desc), CacheType::String, false);
        return default_locs;
    }

    let mut out = run_single_command(&[bin, pcvar.to_owned(), "pkg-config".to_owned()]);
    resolver::replace_sep(&mut out);
    let out = out.trim().to_owned();
    interp.add_cache_def(cachevar, Some(&out), Some(desc), CacheType::String, false);
    expand_list(&out, false)
}

/// `GetPcLibDirs`.
fn get_pc_lib_dirs(interp: &mut Interp) -> Vec<String> {
    let default_locs = if cfg!(windows) {
        Vec::new()
    } else {
        vec![
            "/usr/lib/pkgconfig".to_owned(),
            "/usr/share/pkgconfig".to_owned(),
        ]
    };
    get_locations(
        interp,
        "CMAKE_PKG_CONFIG_PC_LIB_DIRS",
        "PKG_CONFIG_LIBDIR",
        "Default search locations for package files",
        "--variable=pc_path",
        false,
        default_locs,
    )
}

/// `GetSysLibDirs`.
fn get_sys_lib_dirs(interp: &mut Interp) -> Vec<String> {
    let default_locs = if cfg!(windows) {
        Vec::new()
    } else {
        vec!["/lib".to_owned(), "/usr/lib".to_owned()]
    };
    get_locations(
        interp,
        "CMAKE_PKG_CONFIG_SYS_LIB_DIRS",
        "PKG_CONFIG_SYSTEM_LIBRARY_PATH",
        "System library directories filtered by flag mangling",
        "--variable=pc_system_libdirs",
        true,
        default_locs,
    )
}

/// `GetSysCflags`.
fn get_sys_cflags(interp: &mut Interp) -> Vec<String> {
    let default_locs = if cfg!(windows) {
        Vec::new()
    } else {
        vec!["/usr/include".to_owned()]
    };
    get_locations(
        interp,
        "CMAKE_PKG_CONFIG_SYS_INCLUDE_DIRS",
        "PKG_CONFIG_SYSTEM_INCLUDE_PATH",
        "System include directories filtered by flag mangling",
        "--variable=pc_system_includedirs",
        true,
        default_locs,
    )
}

/// `GetPkgConfSysLibs`.
fn get_pkgconf_sys_libs(interp: &mut Interp) -> Vec<String> {
    if let Some(def) = interp.get_def("CMAKE_PKG_CONFIG_PKGCONF_LIB_DIRS").map(str::to_owned) {
        return expand_list(&def, false);
    }

    let Some(mut paths) = get_env("LIBRARY_PATH") else {
        return Vec::new();
    };

    resolver::replace_sep(&mut paths);
    interp.add_cache_def(
        "CMAKE_PKG_CONFIG_PKGCONF_LIB_DIRS",
        Some(&paths),
        Some("Additional system library directories filtered by flag mangling in PKGCONF mode"),
        CacheType::String,
        false,
    );
    expand_list(&paths, false)
}

/// `GetPkgConfSysCflags`.
fn get_pkgconf_sys_cflags(interp: &mut Interp) -> Vec<String> {
    if let Some(def) = interp.get_def("CMAKE_PKG_CONFIG_PKGCONF_INCLUDES").map(str::to_owned) {
        return expand_list(&def, false);
    }

    let mut paths = String::new();
    let mut get_and_append = |var: &str| {
        if paths.is_empty() {
            if let Some(value) = get_env(var) {
                paths = value;
            }
        } else {
            if let Some(tmp) = get_env(var) {
                if !tmp.is_empty() {
                    paths.push(';');
                    paths.push_str(&tmp);
                }
            }
        }
    };

    get_and_append("CPATH");
    get_and_append("C_INCLUDE_PATH");
    get_and_append("CPLUS_INCLUDE_PATH");
    get_and_append("OBJC_INCLUDE_PATH");

    if cfg!(windows) {
        get_and_append("INCLUDE");
    }

    resolver::replace_sep(&mut paths);
    interp.add_cache_def(
        "CMAKE_PKG_CONFIG_PKGCONF_INCLUDES",
        Some(&paths),
        Some("Additional system include directories filtered by flag mangling in PKGCONF mode"),
        CacheType::String,
        false,
    );
    expand_list(&paths, false)
}

/// `GetPcPath`.
fn get_pc_path(interp: &mut Interp) -> Vec<String> {
    if let Some(def) = interp.get_def("CMAKE_PKG_CONFIG_PC_PATH").map(str::to_owned) {
        return expand_list(&def, false);
    }

    if let Some(pcpath) = get_env("PKG_CONFIG_PATH") {
        let result = split_string(&pcpath, if cfg!(windows) { ';' } else { ':' });
        let joined = join_list(&result);
        interp.add_cache_def(
            "CMAKE_PKG_CONFIG_PC_PATH",
            Some(&joined),
            Some("Additional search locations for package files"),
            CacheType::String,
            false,
        );
        return result;
    }

    interp.add_cache_def(
        "CMAKE_PKG_CONFIG_PC_PATH",
        Some(""),
        Some("Additional search locations for package files"),
        CacheType::String,
        false,
    );
    Vec::new()
}

/// `GetPath`.
fn get_path(interp: &mut Interp, cachevar: &str, envvar: &str, desc: &str) -> Option<String> {
    if let Some(def) = interp.get_def(cachevar).map(str::to_owned) {
        return Some(def);
    }

    if let Some(path) = get_env(envvar) {
        interp.add_cache_def(cachevar, Some(&path), Some(desc), CacheType::Filepath, false);
        return Some(path);
    }

    None
}

/// `GetSysrootDir`.
fn get_sysroot_dir(interp: &mut Interp) -> Option<String> {
    get_path(
        interp,
        "CMAKE_PKG_CONFIG_SYSROOT_DIR",
        "PKG_CONFIG_SYSROOT_DIR",
        "System root used for re-rooting package includes and library directories",
    )
}

/// `GetTopBuildDir`.
fn get_top_build_dir(interp: &mut Interp) -> Option<String> {
    get_path(
        interp,
        "CMAKE_PKG_CONFIG_TOP_BUILD_DIR",
        "PKG_CONFIG_TOP_BUILD_DIR",
        "Package file top_build_dir variable default value",
    )
}

/// `GetBool`.
fn get_bool(interp: &mut Interp, cachevar: &str, envvar: &str, desc: &str) -> bool {
    if interp.get_def(cachevar).is_some() {
        return is_on_opt(interp.get_def(cachevar));
    }

    if has_env(envvar) {
        interp.add_cache_def(cachevar, Some("ON"), Some(desc), CacheType::Bool, false);
        return true;
    }

    false
}

/// `GetDisableUninstalled`.
fn get_disable_uninstalled(interp: &mut Interp) -> bool {
    get_bool(
        interp,
        "CMAKE_PKG_CONFIG_DISABLE_UNINSTALLED",
        "PKG_CONFIG_DISABLE_UNINSTALLED",
        "Disable search for `-uninstalled` (build tree) packages",
    )
}

/// `GetAllowSysLibs`.
fn get_allow_sys_libs(interp: &mut Interp) -> bool {
    get_bool(
        interp,
        "CMAKE_PKG_CONFIG_ALLOW_SYS_LIBS",
        "PKG_CONFIG_ALLOW_SYSTEM_LIBS",
        "Allow system library directories during flag mangling",
    )
}

/// `GetAllowSysInclude`.
fn get_allow_sys_include(interp: &mut Interp) -> bool {
    get_bool(
        interp,
        "CMAKE_PKG_CONFIG_ALLOW_SYS_INCLUDES",
        "PKG_CONFIG_ALLOW_SYSTEM_CFLAGS",
        "Allow system include paths during flag manglging",
    )
}

// ---------------------------------------------------------------------------
// Argument parsing (cmArgumentParser semantics)

#[derive(Clone, Copy, PartialEq, Eq)]
enum Strictness {
    Strict,
    Permissive,
    BestEffort,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EnvMode {
    Fdo,
    Pkgconf,
    Ignore,
}

#[derive(Default)]
struct CommonArgs {
    required: bool,
    exact: bool,
    quiet: bool,
    strictness: Strictness,
    strictness_error: String,
    env_mode: EnvMode,
    env_mode_error: String,
    package: Option<String>,
    version: Option<String>,
    sysroot_dir: Option<String>,
    top_build_dir: Option<String>,
    disable_uninstalled: Option<bool>,
    pc_libdir: Option<Vec<String>>,
    pc_path: Option<Vec<String>>,
}

impl Default for Strictness {
    fn default() -> Self {
        Strictness::Permissive
    }
}

impl Default for EnvMode {
    fn default() -> Self {
        EnvMode::Pkgconf
    }
}

impl CommonArgs {
    /// `CommonArguments::SetStrictness`.
    fn set_strictness(&mut self, strictness: &str) {
        match strictness {
            "STRICT" => self.strictness = Strictness::Strict,
            "PERMISSIVE" => self.strictness = Strictness::Permissive,
            "BEST_EFFORT" => self.strictness = Strictness::BestEffort,
            _ => {
                self.strictness_error = format!(
                    "Invalid 'STRICTNESS' '{strictness}'; must be one of 'STRICT', 'PERMISSIVE', or 'BEST_EFFORT'"
                );
            }
        }
    }

    /// `CommonArguments::SetEnvMode`.
    fn set_env_mode(&mut self, env_mode: &str) {
        match env_mode {
            "FDO" => self.env_mode = EnvMode::Fdo,
            "PKGCONF" => self.env_mode = EnvMode::Pkgconf,
            "IGNORE" => self.env_mode = EnvMode::Ignore,
            _ => {
                self.env_mode_error = format!(
                    "Invalid 'ENV_MODE' '{env_mode}'; must be one of 'FDO', 'PKGCONF', or 'IGNORE'"
                );
            }
        }
    }

    /// `CommonArguments::CheckArgs` — returns the `SetError` text.
    fn check_args(&self) -> Option<String> {
        if self.package.is_none() {
            return Some("A package name or absolute path must be specified".to_owned());
        }
        if !self.strictness_error.is_empty() {
            return Some(self.strictness_error.clone());
        }
        if !self.env_mode_error.is_empty() {
            return Some(self.env_mode_error.clone());
        }
        None
    }
}

#[derive(Default)]
struct ExtractArgs {
    common: CommonArgs,
    allow_system_includes: Option<bool>,
    allow_system_libs: Option<bool>,
    system_include_dirs: Option<Vec<String>>,
    system_library_dirs: Option<Vec<String>>,
}

#[derive(Default)]
struct PopulateArgs {
    common: CommonArgs,
    prefix: Option<String>,
    providers: Option<Vec<String>>,
}

#[derive(Default)]
struct ImportArgs {
    populate: PopulateArgs,
    name: Option<String>,
}

/// Keyword binding kinds.  `OptionalBool` is a `cm::optional<bool>` which the
/// upstream parser binds as a zero-value flag that emplaces `true`.
#[derive(Clone, Copy)]
enum Kw {
    Required,
    Exact,
    Quiet,
    Strictness,
    EnvModeKw,
    SysrootDir,
    TopBuildDir,
    DisableUninstalled,
    PcLibdir,
    PcPath,
    AllowSystemIncludes,
    AllowSystemLibs,
    SystemIncludeDirs,
    SystemLibraryDirs,
    Prefix,
    BindPcRequires,
    Name,
}

/// Which single-value or list-value sink the current keyword writes into.
#[derive(Clone, Copy)]
enum Active {
    SysrootDir,
    TopBuildDir,
    Prefix,
    Name,
    Strictness,
    EnvModeKw,
    PcLibdir,
    PcPath,
    SystemIncludeDirs,
    SystemLibraryDirs,
    BindPcRequires,
}

fn common_keywords() -> Vec<(&'static str, Kw)> {
    vec![
        ("REQUIRED", Kw::Required),
        ("EXACT", Kw::Exact),
        ("QUIET", Kw::Quiet),
        ("STRICTNESS", Kw::Strictness),
        ("ENV_MODE", Kw::EnvModeKw),
        ("PC_SYSROOT_DIR", Kw::SysrootDir),
        ("TOP_BUILD_DIR", Kw::TopBuildDir),
        ("DISABLE_UNINSTALLED", Kw::DisableUninstalled),
        ("PC_LIBDIR", Kw::PcLibdir),
        ("PC_PATH", Kw::PcPath),
    ]
}

/// Drives `cmArgumentParser` positional/keyword consumption.  `args` includes
/// the subcommand name at position 0, so positional binds 1 and 2 land on
/// PACKAGE and VERSION.
trait ArgSink {
    fn common(&mut self) -> &mut CommonArgs;
    fn keywords(&self) -> Vec<(&'static str, Kw)>;
    fn set_single(&mut self, kw: Kw, value: String);
    fn set_flag(&mut self, kw: Kw);
    fn list(&mut self, kw: Kw) -> &mut Vec<String>;
}

fn parse_subcommand<S: ArgSink>(args: &[String], sink: &mut S) {
    let keywords: HashMap<&str, Kw> = sink.keywords().into_iter().collect();
    let mut done_with_positional = false;
    let mut active: Option<Active> = None;

    for (pos, arg) in args.iter().enumerate() {
        if let Some(&kw) = keywords.get(arg.as_str()) {
            // FinishKeyword: missing-value errors are never reported by the
            // upstream handlers, so it is a no-op here.
            done_with_positional = true;
            match kw {
                Kw::Required
                | Kw::Exact
                | Kw::Quiet
                | Kw::DisableUninstalled
                | Kw::AllowSystemIncludes
                | Kw::AllowSystemLibs => sink.set_flag(kw),
                Kw::Strictness => active = Some(Active::Strictness),
                Kw::EnvModeKw => active = Some(Active::EnvModeKw),
                Kw::SysrootDir => active = Some(Active::SysrootDir),
                Kw::TopBuildDir => active = Some(Active::TopBuildDir),
                Kw::Prefix => active = Some(Active::Prefix),
                Kw::Name => active = Some(Active::Name),
                Kw::PcLibdir => active = Some(Active::PcLibdir),
                Kw::PcPath => active = Some(Active::PcPath),
                Kw::SystemIncludeDirs => active = Some(Active::SystemIncludeDirs),
                Kw::SystemLibraryDirs => active = Some(Active::SystemLibraryDirs),
                Kw::BindPcRequires => active = Some(Active::BindPcRequires),
            }
            continue;
        }

        if !done_with_positional {
            match pos {
                1 => {
                    sink.common().package = Some(arg.clone());
                    continue;
                }
                2 => {
                    sink.common().version = Some(arg.clone());
                    continue;
                }
                _ => {}
            }
        }

        match active {
            Some(Active::SysrootDir) => {
                sink.common().sysroot_dir = Some(arg.clone());
                active = None;
            }
            Some(Active::TopBuildDir) => {
                sink.common().top_build_dir = Some(arg.clone());
                active = None;
            }
            Some(Active::Prefix) => {
                sink.set_single(Kw::Prefix, arg.clone());
                active = None;
            }
            Some(Active::Name) => {
                sink.set_single(Kw::Name, arg.clone());
                active = None;
            }
            Some(Active::PcLibdir) => sink.list(Kw::PcLibdir).push(arg.clone()),
            Some(Active::PcPath) => sink.list(Kw::PcPath).push(arg.clone()),
            Some(Active::SystemIncludeDirs) => {
                sink.list(Kw::SystemIncludeDirs).push(arg.clone())
            }
            Some(Active::SystemLibraryDirs) => {
                sink.list(Kw::SystemLibraryDirs).push(arg.clone())
            }
            Some(Active::BindPcRequires) => sink.list(Kw::BindPcRequires).push(arg.clone()),
            Some(Active::Strictness) => sink.common().set_strictness(arg),
            Some(Active::EnvModeKw) => sink.common().set_env_mode(arg),
            None => {
                // Unparsed argument; the upstream handlers ignore these.
            }
        }
    }
}

// -- ArgSink implementations ------------------------------------------------

impl ArgSink for ExtractArgs {
    fn common(&mut self) -> &mut CommonArgs {
        &mut self.common
    }

    fn keywords(&self) -> Vec<(&'static str, Kw)> {
        let mut kw = common_keywords();
        kw.extend([
            ("ALLOW_SYSTEM_INCLUDES", Kw::AllowSystemIncludes),
            ("ALLOW_SYSTEM_LIBS", Kw::AllowSystemLibs),
            ("SYSTEM_INCLUDE_DIRS", Kw::SystemIncludeDirs),
            ("SYSTEM_LIBRARY_DIRS", Kw::SystemLibraryDirs),
        ]);
        kw
    }

    fn set_single(&mut self, _kw: Kw, _value: String) {}

    fn set_flag(&mut self, kw: Kw) {
        match kw {
            Kw::Required => self.common.required = true,
            Kw::Exact => self.common.exact = true,
            Kw::Quiet => self.common.quiet = true,
            Kw::DisableUninstalled => self.common.disable_uninstalled = Some(true),
            Kw::AllowSystemIncludes => self.allow_system_includes = Some(true),
            Kw::AllowSystemLibs => self.allow_system_libs = Some(true),
            _ => {}
        }
    }

    fn list(&mut self, kw: Kw) -> &mut Vec<String> {
        match kw {
            Kw::PcLibdir => self.common.pc_libdir.get_or_insert_with(Vec::new),
            Kw::PcPath => self.common.pc_path.get_or_insert_with(Vec::new),
            Kw::SystemIncludeDirs => self.system_include_dirs.get_or_insert_with(Vec::new),
            Kw::SystemLibraryDirs => self.system_library_dirs.get_or_insert_with(Vec::new),
            _ => unreachable!(),
        }
    }
}

impl ArgSink for PopulateArgs {
    fn common(&mut self) -> &mut CommonArgs {
        &mut self.common
    }

    fn keywords(&self) -> Vec<(&'static str, Kw)> {
        let mut kw = common_keywords();
        kw.extend([("PREFIX", Kw::Prefix), ("BIND_PC_REQUIRES", Kw::BindPcRequires)]);
        kw
    }

    fn set_single(&mut self, kw: Kw, value: String) {
        if matches!(kw, Kw::Prefix) {
            self.prefix = Some(value);
        }
    }

    fn set_flag(&mut self, kw: Kw) {
        match kw {
            Kw::Required => self.common.required = true,
            Kw::Exact => self.common.exact = true,
            Kw::Quiet => self.common.quiet = true,
            Kw::DisableUninstalled => self.common.disable_uninstalled = Some(true),
            _ => {}
        }
    }

    fn list(&mut self, kw: Kw) -> &mut Vec<String> {
        match kw {
            Kw::PcLibdir => self.common.pc_libdir.get_or_insert_with(Vec::new),
            Kw::PcPath => self.common.pc_path.get_or_insert_with(Vec::new),
            Kw::BindPcRequires => self.providers.get_or_insert_with(Vec::new),
            _ => unreachable!(),
        }
    }
}

impl ArgSink for ImportArgs {
    fn common(&mut self) -> &mut CommonArgs {
        &mut self.populate.common
    }

    fn keywords(&self) -> Vec<(&'static str, Kw)> {
        let mut kw = common_keywords();
        kw.extend([
            ("PREFIX", Kw::Prefix),
            ("BIND_PC_REQUIRES", Kw::BindPcRequires),
            ("NAME", Kw::Name),
        ]);
        kw
    }

    fn set_single(&mut self, kw: Kw, value: String) {
        match kw {
            Kw::Prefix => self.populate.prefix = Some(value),
            Kw::Name => self.name = Some(value),
            _ => {}
        }
    }

    fn set_flag(&mut self, kw: Kw) {
        self.populate.set_flag(kw)
    }

    fn list(&mut self, kw: Kw) -> &mut Vec<String> {
        self.populate.list(kw)
    }
}

// ---------------------------------------------------------------------------
// HandleCommon / CollectEnv

/// `CollectEnv`.
fn collect_env(interp: &mut Interp, env: &mut Env, mode: EnvMode) {
    if mode == EnvMode::Ignore {
        return;
    }

    if env.path.is_none() {
        env.path = Some(get_pc_path(interp));
    }
    if env.lib_dirs.is_none() {
        env.lib_dirs = Some(get_pc_lib_dirs(interp));
    }
    if env.disable_uninstalled.is_none() {
        env.disable_uninstalled = Some(get_disable_uninstalled(interp));
    }
    if env.sysroot_dir.is_none() {
        env.sysroot_dir = get_sysroot_dir(interp);
    }
    if env.top_build_dir.is_none() {
        env.top_build_dir = get_top_build_dir(interp);
    }

    env.allow_sys_cflags = get_allow_sys_include(interp);
    env.sys_cflags = Some(get_sys_cflags(interp));

    env.allow_sys_libs = get_allow_sys_libs(interp);
    env.sys_libs = Some(get_sys_lib_dirs(interp));

    if mode == EnvMode::Fdo {
        return;
    }

    env.sys_cflags
        .as_mut()
        .unwrap()
        .extend(get_pkgconf_sys_cflags(interp));
    env.sys_libs
        .as_mut()
        .unwrap()
        .extend(get_pkgconf_sys_libs(interp));
}

/// `ImportEnv`: `status_error` models `cmExecutionStatus::SetError`.
struct ImportEnv {
    required: bool,
    quiet: bool,
    exact: bool,
    err: bool,
    strictness: Strictness,
    status_error: Option<String>,
}

/// `warn_or_error`.
fn warn_or_error(interp: &mut Interp, im_env: &mut ImportEnv, error: String) {
    if im_env.required {
        im_env.status_error = Some(error);
        interp.set_fatal();
    } else if !im_env.quiet {
        interp.issue(MessageType::Warning, &error);
    }
    im_env.err = true;
}

/// `HandleCommon`: `Err(msg)` corresponds to `status.SetError(msg)` with a
/// `CheckArgs` failure.
fn handle_common(
    interp: &mut Interp,
    args: &CommonArgs,
) -> Result<(Env, ImportEnv), String> {
    if let Some(msg) = args.check_args() {
        return Err(msg);
    }

    let mut env = Env::default();

    if let Some(libdir) = &args.pc_libdir {
        env.lib_dirs = Some(libdir.clone());
    }
    if let Some(path) = &args.pc_path {
        env.path = Some(path.clone());
    }
    env.disable_uninstalled = args.disable_uninstalled;
    if let Some(sysroot) = &args.sysroot_dir {
        env.sysroot_dir = Some(sysroot.clone());
    }
    if let Some(top) = &args.top_build_dir {
        env.top_build_dir = Some(top.clone());
    }

    collect_env(interp, &mut env, args.env_mode);

    if let Some(path) = &env.path {
        env.search = path.clone();
        if let Some(lib_dirs) = &env.lib_dirs {
            env.search.extend(lib_dirs.iter().cloned());
        }
    } else if let Some(lib_dirs) = &env.lib_dirs {
        env.search = lib_dirs.clone();
    }

    Ok((
        env,
        ImportEnv {
            required: args.required,
            quiet: args.quiet,
            exact: args.exact,
            err: false,
            strictness: args.strictness,
            status_error: None,
        },
    ))
}

// ---------------------------------------------------------------------------
// ReadPackage / ImportPackage

/// `ReadPackage`.
fn read_package(
    interp: &mut Interp,
    package: &str,
    im_env: &mut ImportEnv,
    pc_env: &Env,
) -> Option<PkgConfig> {
    let path = if Path::new(package).extension().is_some_and(|ext| ext == "pc") {
        if !file_exists(package) {
            return None;
        }
        package.to_owned()
    } else if pc_env.disable_uninstalled.is_some_and(|disable| !disable) {
        let uninstalled = find_file(&format!("{package}-uninstalled.pc"), &pc_env.search);
        if uninstalled.is_empty() {
            let found = find_file(&format!("{package}.pc"), &pc_env.search);
            if found.is_empty() {
                return None;
            }
            found
        } else {
            uninstalled
        }
    } else {
        let found = find_file(&format!("{package}.pc"), &pc_env.search);
        if found.is_empty() {
            return None;
        }
        found
    };

    // Upstream: FileLength -> open ifstream -> read exactly `len` bytes.
    let mut buf = Vec::new();
    match std::fs::File::open(&path) {
        Ok(mut f) => {
            if f.read_to_end(&mut buf).is_err() {
                warn_or_error(interp, im_env, format!("Error while reading file '{path}'"));
                return None;
            }
        }
        Err(_) => {
            warn_or_error(interp, im_env, format!("Could not open file '{path}'"));
            return None;
        }
    }

    let mut parser = Parser::new();
    let err_code = parser.finish_with(&buf);

    if im_env.strictness != Strictness::BestEffort && err_code != PCE_OK {
        warn_or_error(interp, im_env, format!("Parsing failed for file '{path}'"));
        return None;
    }

    let result = match im_env.strictness {
        Strictness::Strict => resolver::resolve_strict(parser.data(), pc_env),
        Strictness::Permissive => resolver::resolve_permissive(parser.data(), pc_env),
        Strictness::BestEffort => Some(resolver::resolve_best_effort(parser.data(), pc_env)),
    };

    if result.is_none() {
        warn_or_error(interp, im_env, format!("Resolution failed for file '{path}'"));
    }

    result
}

/// `ImportPackage(package, version, imEnv, pcEnv)`.
fn import_package(
    interp: &mut Interp,
    package: &str,
    version: Option<&str>,
    im_env: &mut ImportEnv,
    pc_env: &Env,
) -> Option<PkgConfig> {
    let result = read_package(interp, package, im_env, pc_env);

    let Some(pkg) = result else {
        if !im_env.err {
            warn_or_error(interp, im_env, format!("Could not find pkg-config: '{package}'"));
        }
        return None;
    };

    if im_env.exact {
        let ver = version.map(|v| parse_version(v).version).unwrap_or_default();
        if ver != pkg.version() {
            warn_or_error(
                interp,
                im_env,
                format!(
                    "Package '{package}' version '{}' does not meet exact version requirement '{ver}'",
                    pkg.version()
                ),
            );
            return None;
        }
    } else if let Some(version) = version {
        let rv = parse_version(version);
        if !check_version(&rv, &pkg.version()) {
            warn_or_error(
                interp,
                im_env,
                format!(
                    "Package '{package}' version '{}' does not meet version requirement '{version}'",
                    pkg.version()
                ),
            );
            return None;
        }
    }

    Some(pkg)
}

/// `pkgStackEntry`.
#[derive(Clone)]
struct PkgStackEntry {
    ver: VersionReq,
    parent: String,
}

/// `ImportPackage(package, reqs, imEnv, pcEnv)` — the transitive overload.
fn import_package_reqs(
    interp: &mut Interp,
    package: &str,
    reqs: &[PkgStackEntry],
    im_env: &mut ImportEnv,
    pc_env: &Env,
) -> Option<PkgConfig> {
    let result = read_package(interp, package, im_env, pc_env);

    let Some(pkg) = result else {
        if !im_env.err {
            let mut req_str = format!("'{}'", reqs[0].parent);
            for req in &reqs[1..] {
                req_str += &format!(", '{}'", req.parent);
            }
            warn_or_error(
                interp,
                im_env,
                format!("Could not find pkg-config: '{package}' required by: {req_str}"),
            );
        }
        return None;
    };

    let ver = pkg.version();
    for req in reqs {
        if !check_version(&req.ver, &ver) {
            warn_or_error(
                interp,
                im_env,
                format!(
                    "Package '{package}' version '{ver}' does not meet version requirement '{}' of '{}'",
                    req.ver.string(),
                    req.parent
                ),
            );
            return None;
        }
    }

    Some(pkg)
}

// ---------------------------------------------------------------------------
// POPULATE machinery

type PkgProviders = HashMap<String, String>;
type PkgStack = Vec<(String, Vec<PkgStackEntry>)>;
type OutStack = Vec<(String, PkgConfig)>;

fn out_stack_get<'a>(out_stack: &'a OutStack, name: &str) -> Option<&'a PkgConfig> {
    out_stack
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, pkg)| pkg)
}

fn in_stack_push(in_stack: &mut PkgStack, name: String, entry: PkgStackEntry) {
    if let Some((_, entries)) = in_stack.iter_mut().find(|(n, _)| n == &name) {
        entries.push(entry);
    } else {
        in_stack.push((name, vec![entry]));
    }
}

/// `CreateCMakeTarget`.  `mf.AddForeignTarget("pkgcfg", prefix + name)` creates
/// an interface-library target named `@foreign_pkgcfg::<prefix><name>`; the
/// Rust port has no foreign domain so an ordinary imported target is used.
fn create_cmake_target(
    interp: &mut Interp,
    name: &str,
    prefix: &str,
    pkg: &PkgConfig,
    providers: &PkgProviders,
    pc_env: &Env,
) {
    let full_name = format!("@foreign_pkgcfg::{prefix}{name}");
    let key = crate::commands::target::create_target(
        interp,
        &full_name,
        TargetKind::InterfaceLibrary,
        true,
        false,
    );

    // Resolve the stored target back (key may differ from the display name).
    let Some(target) = interp.ext.targets.get_mut(&key) else {
        return;
    };

    target.props.append("VERSION", &pkg.version(), false);

    let libs = pkg.libs(pc_env, false);
    for flag in &libs.lib_names {
        target
            .props
            .append("INTERFACE_LINK_LIBRARIES", &flag[2..], false);
    }
    for flag in &libs.lib_dirs {
        target
            .props
            .append("INTERFACE_LINK_DIRECTORIES", &flag[2..], false);
    }
    target
        .props
        .append("INTERFACE_LINK_OPTIONS", &join_list(&libs.link_options), false);

    let cflags = pkg.cflags(pc_env, false);
    for flag in &cflags.includes {
        target
            .props
            .append("INTERFACE_INCLUDE_DIRECTORIES", &flag[2..], false);
    }
    target
        .props
        .append(
            "INTERFACE_COMPILE_OPTIONS",
            &join_list(&cflags.compile_options),
            false,
        );

    for dep in pkg.requires(false) {
        if let Some(target_name) = providers.get(&dep.name) {
            target
                .props
                .append("INTERFACE_LINK_LIBRARIES", target_name, false);
            continue;
        }
        target.props.append(
            "INTERFACE_LINK_LIBRARIES",
            &format!("@foreign_pkgcfg::{prefix}{}", dep.name),
            false,
        );
    }
}

/// `CheckPackageDependencies`.
fn check_package_dependencies(
    interp: &mut Interp,
    name: &str,
    prefix: &str,
    pkg: &PkgConfig,
    in_stack: &mut PkgStack,
    out_stack: &OutStack,
    providers: &PkgProviders,
    im_env: &mut ImportEnv,
) -> bool {
    for dep in pkg.requires(false) {
        if providers.contains_key(&dep.name) {
            continue;
        }

        let foreign = format!("@foreign_pkgcfg::{prefix}{}", dep.name);
        let existing_ver = find_target_to_use(interp, &foreign)
            .map(|t| t.props.get("VERSION").unwrap_or("").to_owned());
        if let Some(ver) = existing_ver {
            if !check_version(&dep.ver_req, &ver) {
                warn_or_error(
                    interp,
                    im_env,
                    format!(
                        "Package '{}' version '{ver}' does not meet version requirement '{}' of '{name}'",
                        dep.name,
                        dep.ver_req.string()
                    ),
                );
                return false;
            }
            continue;
        }

        if let Some(existing) = out_stack_get(out_stack, &dep.name) {
            let ver = existing.version();
            if !check_version(&dep.ver_req, &ver) {
                warn_or_error(
                    interp,
                    im_env,
                    format!(
                        "Package '{}' version '{ver}' does not meet version requirement '{}' of '{name}'",
                        dep.name,
                        dep.ver_req.string()
                    ),
                );
                return false;
            }
            continue;
        }

        in_stack_push(
            in_stack,
            dep.name.clone(),
            PkgStackEntry {
                ver: dep.ver_req.clone(),
                parent: name.to_owned(),
            },
        );
    }

    true
}

/// `PopulatePCTarget`: returns (command success, found, status error).
fn populate_pc_target(
    interp: &mut Interp,
    args: &PopulateArgs,
) -> (bool, bool, Option<String>) {
    let prefix = args
        .prefix
        .as_ref()
        .map_or(String::new(), |p| format!("{p}_"));

    let (pc_env, mut im_env) = match handle_common(interp, &args.common) {
        Ok(env) => env,
        Err(msg) => return (!args.common.required, false, Some(msg)),
    };

    let mut providers = PkgProviders::new();
    if let Some(provider_strs) = &args.providers {
        for provider_str in provider_strs {
            match provider_str.find('=') {
                Some(assignment) => {
                    providers.insert(
                        provider_str[..assignment].to_owned(),
                        provider_str[assignment + 1..].to_owned(),
                    );
                }
                None => {
                    let msg =
                        format!("No '=' found in BIND_PC_REQUIRES argument '{provider_str}'");
                    im_env.status_error = Some(msg.clone());
                    interp.set_fatal();
                    return (false, false, Some(msg));
                }
            }
        }
    }

    let mut in_stack: PkgStack = Vec::new();
    let mut out_stack: OutStack = Vec::new();

    let package = args.common.package.clone().unwrap_or_default();
    let maybe_package = import_package(
        interp,
        &package,
        args.common.version.as_deref(),
        &mut im_env,
        &pc_env,
    );
    let Some(pkg) = maybe_package else {
        return (!args.common.required, false, im_env.status_error);
    };
    im_env.exact = false;

    if !check_package_dependencies(
        interp,
        &package,
        &prefix,
        &pkg,
        &mut in_stack,
        &out_stack,
        &providers,
        &mut im_env,
    ) {
        return (!args.common.required, false, im_env.status_error);
    }
    out_stack.push((package, pkg));

    while let Some((name, reqs)) = in_stack.first().cloned() {
        let maybe_pkg = import_package_reqs(interp, &name, &reqs, &mut im_env, &pc_env);
        let Some(pkg) = maybe_pkg else {
            return (!args.common.required, false, im_env.status_error);
        };
        if !check_package_dependencies(
            interp,
            &name,
            &prefix,
            &pkg,
            &mut in_stack,
            &out_stack,
            &providers,
            &mut im_env,
        ) {
            return (!args.common.required, false, im_env.status_error);
        }
        in_stack.retain(|(n, _)| n != &name);
        out_stack.push((name, pkg));
    }

    for (name, pkg) in &out_stack {
        create_cmake_target(interp, name, &prefix, pkg, &providers, &pc_env);
    }

    (true, true, None)
}

// ---------------------------------------------------------------------------
// Subcommand handlers

/// `HandleExtractCommand`.
fn handle_extract(interp: &mut Interp, args: &[String]) -> CmdResult {
    let mut parsed = ExtractArgs::default();
    parse_subcommand(args, &mut parsed);

    let (mut pc_env, mut im_env) = match handle_common(interp, &parsed.common) {
        Ok(env) => env,
        Err(msg) => {
            return if parsed.common.required { err(msg) } else { Ok(()) };
        }
    };

    let package = parsed.common.package.clone().unwrap_or_default();
    let maybe_package = import_package(
        interp,
        &package,
        parsed.common.version.as_deref(),
        &mut im_env,
        &pc_env,
    );
    let Some(pkg) = maybe_package else {
        return if parsed.common.required {
            err(im_env.status_error.unwrap_or_default())
        } else {
            Ok(())
        };
    };

    if let Some(allow) = parsed.allow_system_includes {
        pc_env.allow_sys_cflags = allow;
    }
    if let Some(allow) = parsed.allow_system_libs {
        pc_env.allow_sys_libs = allow;
    }
    if let Some(dirs) = parsed.system_include_dirs {
        pc_env.sys_cflags = Some(dirs);
    }
    if let Some(dirs) = parsed.system_library_dirs {
        pc_env.sys_libs = Some(dirs);
    }

    interp.set_def("CMAKE_PKG_CONFIG_NAME", pkg.name());
    interp.set_def("CMAKE_PKG_CONFIG_DESCRIPTION", pkg.description());
    interp.set_def("CMAKE_PKG_CONFIG_VERSION", pkg.version());

    let make_list = |deps: Vec<resolver::Dependency>| -> String {
        join_list(&deps.into_iter().map(|d| d.name).collect::<Vec<_>>())
    };

    interp.set_def("CMAKE_PKG_CONFIG_CONFLICTS", make_list(pkg.conflicts()));
    interp.set_def("CMAKE_PKG_CONFIG_PROVIDES", make_list(pkg.provides()));
    interp.set_def("CMAKE_PKG_CONFIG_REQUIRES", make_list(pkg.requires(false)));
    interp.set_def(
        "CMAKE_PKG_CONFIG_REQUIRES_PRIVATE",
        make_list(pkg.requires(true)),
    );

    let cflags = pkg.cflags(&pc_env, false);
    interp.set_def(
        "CMAKE_PKG_CONFIG_CFLAGS",
        String::from_utf8_lossy(&cflags.flagline).into_owned(),
    );
    interp.set_def(
        "CMAKE_PKG_CONFIG_INCLUDES",
        join_list(&cflags.includes),
    );
    interp.set_def(
        "CMAKE_PKG_CONFIG_COMPILE_OPTIONS",
        join_list(&cflags.compile_options),
    );

    let cflags = pkg.cflags(&pc_env, true);
    interp.set_def(
        "CMAKE_PKG_CONFIG_CFLAGS_PRIVATE",
        String::from_utf8_lossy(&cflags.flagline).into_owned(),
    );
    interp.set_def(
        "CMAKE_PKG_CONFIG_INCLUDES_PRIVATE",
        join_list(&cflags.includes),
    );
    interp.set_def(
        "CMAKE_PKG_CONFIG_COMPILE_OPTIONS_PRIVATE",
        join_list(&cflags.compile_options),
    );

    let libs = pkg.libs(&pc_env, false);
    interp.set_def(
        "CMAKE_PKG_CONFIG_LIBS",
        String::from_utf8_lossy(&libs.flagline).into_owned(),
    );
    interp.set_def("CMAKE_PKG_CONFIG_LIBDIRS", join_list(&libs.lib_dirs));
    interp.set_def("CMAKE_PKG_CONFIG_LIBNAMES", join_list(&libs.lib_names));
    interp.set_def(
        "CMAKE_PKG_CONFIG_LINK_OPTIONS",
        join_list(&libs.link_options),
    );

    let libs = pkg.libs(&pc_env, true);
    interp.set_def(
        "CMAKE_PKG_CONFIG_LIBS_PRIVATE",
        String::from_utf8_lossy(&libs.flagline).into_owned(),
    );
    interp.set_def(
        "CMAKE_PKG_CONFIG_LIBDIRS_PRIVATE",
        join_list(&libs.lib_dirs),
    );
    interp.set_def(
        "CMAKE_PKG_CONFIG_LIBNAMES_PRIVATE",
        join_list(&libs.lib_names),
    );
    interp.set_def(
        "CMAKE_PKG_CONFIG_LINK_OPTIONS_PRIVATE",
        join_list(&libs.link_options),
    );

    Ok(())
}

/// `HandlePopulateCommand`.
fn handle_populate(interp: &mut Interp, args: &[String]) -> CmdResult {
    let mut parsed = PopulateArgs::default();
    parse_subcommand(args, &mut parsed);

    let prefix = parsed
        .prefix
        .as_ref()
        .map_or(String::new(), |p| format!("{p}_"));
    let package = parsed.common.package.clone().unwrap_or_default();

    let foreign_name = format!("@foreign_pkgcfg::{prefix}{package}");
    let found_var = format!("PKGCONFIG_{package}_FOUND");

    if find_target_to_use(interp, &foreign_name).is_some() {
        interp.set_def(&found_var, "TRUE");
        return Ok(());
    }

    let (ok, found, status_error) = populate_pc_target(interp, &parsed);
    interp.set_def(&found_var, if found { "TRUE" } else { "FALSE" });
    if ok {
        Ok(())
    } else {
        err(status_error.unwrap_or_default())
    }
}

/// `HandleImportCommand`.
fn handle_import(interp: &mut Interp, args: &[String]) -> CmdResult {
    let mut parsed = ImportArgs::default();
    parse_subcommand(args, &mut parsed);

    let prefix = parsed
        .populate
        .prefix
        .as_ref()
        .map_or(String::new(), |p| format!("{p}_"));
    let package = parsed.populate.common.package.clone().unwrap_or_default();

    let foreign_name = format!("@foreign_pkgcfg::{prefix}{package}");
    let local_name = format!(
        "PkgConfig::{}",
        parsed.name.clone().unwrap_or_else(|| package.clone())
    );
    let found_var = format!("PKGCONFIG_{package}_FOUND");

    if find_target_to_use(interp, &local_name).is_some() {
        interp.set_def(&found_var, "TRUE");
        return Ok(());
    }

    if find_target_to_use(interp, &foreign_name).is_none() {
        let (ok, found, status_error) = populate_pc_target(interp, &parsed.populate);
        if !found {
            interp.set_def(&found_var, "FALSE");
            return if ok { Ok(()) } else { err(status_error.unwrap_or_default()) };
        }
    }

    interp.set_def(&found_var, "TRUE");
    let key = crate::commands::target::create_target(
        interp,
        &local_name,
        TargetKind::InterfaceLibrary,
        true,
        false,
    );
    if let Some(target) = interp.ext.targets.get_mut(&key) {
        target
            .props
            .append("INTERFACE_LINK_LIBRARIES", &foreign_name, false);
    }
    Ok(())
}

/// `cmCMakePkgConfigCommand`.
fn cmake_pkg_config(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 2 {
        return err("must be called with at least two arguments.");
    }

    match args[0].as_str() {
        "EXTRACT" => handle_extract(interp, args),
        "POPULATE" => handle_populate(interp, args),
        "IMPORT" => handle_import(interp, args),
        other => err(format!("does not recognize sub-command {other}")),
    }
}
