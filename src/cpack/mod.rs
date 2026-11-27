//! CPack driver and generator core, ported from `Source/CPack/cpack.cxx`
//! and `cmCPackGenerator.cxx`.

mod archive;
mod deb;
mod external;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::Command;
use std::rc::Rc;

use crate::interp::{Interp, Mode, TraceConfig};
use crate::util;

const VERSION_OUTPUT: &str =
    "cpack version 4.1.0\n\nCMake suite maintained and supported by Kitware (kitware.com/cmake).\n";

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
#[allow(clippy::enum_variant_names)]
enum Method {
    OnePackage,
    OnePackagePerComponent,
    #[default]
    OnePackagePerGroup,
}

impl Method {
    fn upstream_name(self) -> &'static str {
        match self {
            Self::OnePackage => "ALL_COMPONENTS_IN_ONE",
            Self::OnePackagePerComponent => "IGNORE",
            Self::OnePackagePerGroup => "ONE_PER_GROUP",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Archive7z,
    Tbz2,
    Tgz,
    Txz,
    Tz,
    Tzst,
    Tar,
    Zip,
    Stgz,
    Deb,
    Rpm,
    External,
    NuGet,
    Ifw,
    InnoSetup,
    Nsis,
}

#[derive(Clone, Copy)]
struct GeneratorInfo {
    kind: Kind,
}

fn generator_info(name: &str) -> Option<GeneratorInfo> {
    Some(match name {
        "7Z" => GeneratorInfo {
            kind: Kind::Archive7z,
        },
        "TBZ2" => GeneratorInfo { kind: Kind::Tbz2 },
        "TGZ" => GeneratorInfo { kind: Kind::Tgz },
        "TXZ" => GeneratorInfo { kind: Kind::Txz },
        "TZ" => GeneratorInfo { kind: Kind::Tz },
        "TZST" => GeneratorInfo { kind: Kind::Tzst },
        "TAR" => GeneratorInfo { kind: Kind::Tar },
        "ZIP" => GeneratorInfo { kind: Kind::Zip },
        "STGZ" => GeneratorInfo { kind: Kind::Stgz },
        "INNOSETUP" => GeneratorInfo {
            kind: Kind::InnoSetup,
        },
        "NSIS" => GeneratorInfo { kind: Kind::Nsis },
        "NSIS64" => GeneratorInfo { kind: Kind::Nsis },
        "IFW" => GeneratorInfo { kind: Kind::Ifw },
        "DEB" => GeneratorInfo { kind: Kind::Deb },
        "NuGet" => GeneratorInfo { kind: Kind::NuGet },
        "External" => GeneratorInfo {
            kind: Kind::External,
        },
        "RPM" => GeneratorInfo { kind: Kind::Rpm },
        _ => return None,
    })
}

fn generator_list() -> Vec<(&'static str, &'static str)> {
    vec![
        ("7Z", "7-Zip file format"),
        ("DEB", "Debian packages"),
        ("External", "CPack External packages"),
        ("IFW", "Qt Installer Framework"),
        ("INNOSETUP", "Inno Setup packages"),
        ("NSIS", "Null Soft Installer"),
        ("NSIS64", "Null Soft Installer (64-bit)"),
        ("NuGet", "NuGet packages"),
        ("RPM", "RPM packages"),
        ("STGZ", "Self extracting Tar GZip compression"),
        ("TAR", "Tar no compression"),
        ("TBZ2", "Tar BZip2 compression"),
        ("TGZ", "Tar GZip compression"),
        ("TXZ", "Tar XZ compression"),
        ("TZ", "Tar Compress compression"),
        ("TZST", "Tar Zstandard compression"),
        ("ZIP", "ZIP file format"),
    ]
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LogKind {
    Output,
    Verbose,
    Debug,
    Warning,
    Error,
}

struct Logger {
    verbose: bool,
    debug: bool,
    new_line: bool,
    error_occurred: bool,
}

impl Logger {
    fn new() -> Self {
        Self {
            verbose: false,
            debug: false,
            new_line: true,
            error_occurred: false,
        }
    }

    fn log(&mut self, kind: LogKind, text: &str) {
        let show = match kind {
            LogKind::Output | LogKind::Warning | LogKind::Error => true,
            LogKind::Verbose => self.verbose,
            LogKind::Debug => self.debug,
        };
        if !show {
            return;
        }
        let prefix = if self.new_line {
            match kind {
                LogKind::Output => "CPack: ",
                LogKind::Verbose => "CPack Verbose: ",
                LogKind::Debug => "CPack Debug: ",
                LogKind::Warning => "CPack Warning: ",
                LogKind::Error => "CPack Error: ",
            }
        } else {
            ""
        };
        match kind {
            LogKind::Warning | LogKind::Error => {
                eprint!("{prefix}{text}");
            }
            _ => print!("{prefix}{text}"),
        }
        self.new_line = text.ends_with('\n') || text.len() > 2;
        if kind == LogKind::Error {
            self.error_occurred = true;
        }
    }

    fn output(&mut self, text: &str) {
        self.log(LogKind::Output, &format!("{text}\n"));
    }
    fn verbose(&mut self, text: &str) {
        self.log(LogKind::Verbose, &format!("{text}\n"));
    }
    fn debug(&mut self, text: &str) {
        self.log(LogKind::Debug, &format!("{text}\n"));
    }
    fn warning(&mut self, text: &str) {
        self.log(LogKind::Warning, &format!("{text}\n"));
    }
    fn error(&mut self, text: &str) {
        self.log(LogKind::Error, &format!("{text}\n"));
    }
}

#[derive(Default, Clone, Debug)]
struct InstallationType {
    name: String,
    display_name: String,
    index: usize,
}

#[derive(Default, Clone, Debug)]
struct Component {
    name: String,
    display_name: String,
    group: Option<String>,
    required: bool,
    hidden: bool,
    disabled_by_default: bool,
    downloaded: bool,
    description: String,
    installation_types: Vec<String>,
    archive_file: String,
    dependencies: Vec<String>,
    files: Vec<String>,
}

#[derive(Default, Clone, Debug)]
struct ComponentGroup {
    name: String,
    display_name: String,
    description: String,
    bold: bool,
    expanded_by_default: bool,
    parent_group: Option<String>,
    components: Vec<String>,
}

#[derive(Default, Clone, Debug)]
struct CMakeProject {
    directory: String,
    project_name: String,
    component: String,
    sub_directory: String,
    installation_types: Vec<String>,
    components: Vec<String>,
}

struct Generator {
    name: String,
    kind: Kind,
    method: Method,
    components: BTreeMap<String, Component>,
    groups: BTreeMap<String, ComponentGroup>,
    installation_types: BTreeMap<String, InstallationType>,
    cmake_projects: Vec<CMakeProject>,
    files: Vec<String>,
    toplevel: String,
    package_file_names: Vec<String>,
    trace: bool,
    trace_expand: bool,
}

fn option(interp: &Interp, name: &str) -> Option<String> {
    interp.get_def(name).map(str::to_owned)
}

fn option_nonempty(interp: &Interp, name: &str) -> Option<String> {
    option(interp, name).filter(|v| !v.is_empty() && !util::is_notfound(v))
}

fn is_on(interp: &Interp, name: &str) -> bool {
    option(interp, name).is_some_and(|v| util::is_on(&v))
}

fn internally_on(value: Option<&str>) -> bool {
    value.is_some_and(|v| v == "I_ON")
}

fn set_option(interp: &mut Interp, name: &str, value: &str) {
    interp.set_def(name, value);
}

fn set_if_not_set(interp: &mut Interp, name: &str, value: &str) {
    if option_nonempty(interp, name).is_none() {
        interp.set_def(name, value);
    }
}

fn list_option(interp: &Interp, name: &str) -> Vec<String> {
    util::expand_list(option(interp, name).as_deref().unwrap_or(""), false)
}

fn find_modules_file(interp: &Interp, module: &str) -> String {
    for dir in list_option(interp, "CMAKE_MODULE_PATH") {
        let path = format!("{dir}/{module}");
        if Path::new(&path).is_file() {
            return path;
        }
    }
    format!("{}/Modules/{module}", interp.cmake_root)
}

fn read_module(interp: &mut Interp, module: &str) -> bool {
    let path = find_modules_file(interp, module);
    interp.read_list_file(&path) && !interp.error && !interp.fatal
}

fn permission_mode(text: &str) -> Option<u32> {
    Some(match text {
        "OWNER_READ" => 0o400,
        "OWNER_WRITE" => 0o200,
        "OWNER_EXECUTE" => 0o100,
        "GROUP_READ" => 0o040,
        "GROUP_WRITE" => 0o020,
        "GROUP_EXECUTE" => 0o010,
        "WORLD_READ" => 0o004,
        "WORLD_WRITE" => 0o002,
        "WORLD_EXECUTE" => 0o001,
        "SETUID" => 0o4000,
        "SETGID" => 0o2000,
        _ => return None,
    })
}

fn default_dir_mode(interp: &Interp, log: &mut Logger) -> Result<Option<u32>, ()> {
    let Some(value) = option_nonempty(interp, "CPACK_INSTALL_DEFAULT_DIRECTORY_PERMISSIONS") else {
        return Ok(None);
    };
    let mut mode = 0;
    for item in util::expand_list(&value, false) {
        let Some(bit) = permission_mode(&item) else {
            log.error(&format!(
                "Invalid permission value '{item}'. CPACK_INSTALL_DEFAULT_DIRECTORY_PERMISSIONS value is invalid."
            ));
            return Err(());
        };
        mode |= bit;
    }
    Ok(Some(mode))
}

fn make_directory(path: &str, mode: Option<u32>) -> bool {
    if path.is_empty() {
        return false;
    }
    if fs::create_dir_all(path).is_err() {
        return false;
    }
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
    }
    true
}

fn remove_dir_all(path: &str) -> bool {
    !Path::new(path).exists() || fs::remove_dir_all(path).is_ok()
}

/// Recursive `cmsys::Glob` for `<dir>/*`: files and directories, but not
/// through symlinked directories. Paths are absolute and sorted per directory.
fn recurse_files(dir: &str) -> Vec<String> {
    fn visit(path: &Path, out: &mut Vec<String>) {
        let Ok(read) = fs::read_dir(path) else { return };
        for entry in read.flatten() {
            let p = entry.path();
            out.push(p.to_string_lossy().replace('\\', "/"));
            let meta = fs::symlink_metadata(&p);
            if meta.is_ok_and(|m| m.file_type().is_dir()) {
                visit(&p, out);
            }
        }
    }
    let mut out = Vec::new();
    if Path::new(dir).is_dir() {
        visit(Path::new(dir), &mut out);
    }
    out
}

fn copy_file_if_different(source: &str, dest: &str) -> bool {
    if let (Ok(a), Ok(b)) = (fs::read(source), fs::read(dest))
        && a == b
    {
        return true;
    }
    if let Some(parent) = Path::new(dest).parent()
        && fs::create_dir_all(parent).is_err()
    {
        return false;
    }
    fs::copy(source, dest).is_ok()
}

fn relative_path(base: &str, path: &str) -> String {
    util::relative_path(base, path)
        .trim_start_matches("./")
        .to_owned()
}

fn xml_safe(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn sanitize_name(name: &str, full: bool) -> String {
    if full && name.ends_with('.') {
        return format!("{name}_");
    }
    if name.chars().any(|c| "<>\"/\\|?*`".contains(c)) {
        return md5_text(name);
    }
    name.to_owned()
}

trait Hasher {
    fn create() -> Self;
    fn feed(&mut self, data: &[u8]);
    fn finish(self) -> Vec<u8>;
}

impl Hasher for md5::Md5 {
    fn create() -> Self {
        <Self as md5::Digest>::new()
    }
    fn feed(&mut self, data: &[u8]) {
        <Self as md5::Digest>::update(self, data)
    }
    fn finish(self) -> Vec<u8> {
        <Self as md5::Digest>::finalize(self).to_vec()
    }
}
impl Hasher for sha1::Sha1 {
    fn create() -> Self {
        <Self as sha1::Digest>::new()
    }
    fn feed(&mut self, data: &[u8]) {
        <Self as sha1::Digest>::update(self, data)
    }
    fn finish(self) -> Vec<u8> {
        <Self as sha1::Digest>::finalize(self).to_vec()
    }
}
macro_rules! impl_sha2_hasher {
    ($($t:ty);+) => {$(
        impl Hasher for $t {
            fn create() -> Self { <Self as sha2::Digest>::new() }
            fn feed(&mut self, data: &[u8]) { <Self as sha2::Digest>::update(self, data) }
            fn finish(self) -> Vec<u8> { <Self as sha2::Digest>::finalize(self).to_vec() }
        }
    )+};
}
impl_sha2_hasher!(sha2::Sha224; sha2::Sha256; sha2::Sha384; sha2::Sha512);
macro_rules! impl_sha3_hasher {
    ($($t:ty);+) => {$(
        impl Hasher for $t {
            fn create() -> Self { <Self as sha3::Digest>::new() }
            fn feed(&mut self, data: &[u8]) { <Self as sha3::Digest>::update(self, data) }
            fn finish(self) -> Vec<u8> { <Self as sha3::Digest>::finalize(self).to_vec() }
        }
    )+};
}
impl_sha3_hasher!(sha3::Sha3_224; sha3::Sha3_256; sha3::Sha3_384; sha3::Sha3_512);

fn hash_file_typed<H: Hasher>(path: &str) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let mut hasher = H::create();
    let mut buf = [0u8; 32768];
    loop {
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.feed(&buf[..n]);
    }
    Some(hasher.finish().iter().map(|b| format!("{b:02x}")).collect())
}

fn md5_text(text: &str) -> String {
    let mut hasher = <md5::Md5 as md5::Digest>::new();
    <md5::Md5 as md5::Digest>::update(&mut hasher, text.as_bytes());
    <md5::Md5 as md5::Digest>::finalize(hasher)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn file_hash(path: &str, algorithm: &str) -> Option<String> {
    match algorithm.to_ascii_uppercase().as_str() {
        "MD5" => hash_file_typed::<md5::Md5>(path),
        "SHA1" => hash_file_typed::<sha1::Sha1>(path),
        "SHA224" => hash_file_typed::<sha2::Sha224>(path),
        "SHA256" => hash_file_typed::<sha2::Sha256>(path),
        "SHA384" => hash_file_typed::<sha2::Sha384>(path),
        "SHA512" => hash_file_typed::<sha2::Sha512>(path),
        "SHA3_224" => hash_file_typed::<sha3::Sha3_224>(path),
        "SHA3_256" => hash_file_typed::<sha3::Sha3_256>(path),
        "SHA3_384" => hash_file_typed::<sha3::Sha3_384>(path),
        "SHA3_512" => hash_file_typed::<sha3::Sha3_512>(path),
        _ => None,
    }
}

fn valid_checksum_algorithm(name: &str) -> bool {
    matches!(
        name.to_ascii_uppercase().as_str(),
        "MD5"
            | "SHA1"
            | "SHA224"
            | "SHA256"
            | "SHA384"
            | "SHA512"
            | "SHA3_224"
            | "SHA3_256"
            | "SHA3_384"
            | "SHA3_512"
    )
}

impl Generator {
    fn new(name: &str, kind: Kind, trace: bool, trace_expand: bool) -> Self {
        Self {
            name: name.to_owned(),
            kind,
            method: Method::OnePackagePerGroup,
            components: BTreeMap::new(),
            groups: BTreeMap::new(),
            installation_types: BTreeMap::new(),
            cmake_projects: Vec::new(),
            files: Vec::new(),
            toplevel: String::new(),
            package_file_names: Vec::new(),
            trace,
            trace_expand,
        }
    }

    fn class_name(&self) -> &'static str {
        match self.kind {
            Kind::Archive7z
            | Kind::Tbz2
            | Kind::Tgz
            | Kind::Txz
            | Kind::Tz
            | Kind::Tzst
            | Kind::Tar
            | Kind::Zip => "cmCPackArchiveGenerator",
            Kind::Stgz => "cmCPackSTGZGenerator",
            Kind::Deb => "cmCPackDebGenerator",
            Kind::Rpm => "cmCPackRPMGenerator",
            Kind::External => "cmCPackExternalGenerator",
            Kind::NuGet => "cmCPackNuGetGenerator",
            Kind::Ifw => "cmCPackIFWGenerator",
            Kind::InnoSetup => "cmCPackInnoSetupGenerator",
            Kind::Nsis => "cmCPackNSISGenerator",
        }
    }

    fn output_extension(&self, _interp: &Interp) -> &'static str {
        match self.kind {
            Kind::Archive7z => ".7z",
            Kind::Tbz2 => ".tar.bz2",
            Kind::Tgz => ".tar.gz",
            Kind::Txz => ".tar.xz",
            Kind::Tz => ".tar.Z",
            Kind::Tzst => ".tar.zst",
            Kind::Tar => ".tar",
            Kind::Zip => ".zip",
            Kind::Stgz => ".sh",
            Kind::Deb => ".deb",
            Kind::Rpm => ".rpm",
            Kind::External => ".json",
            Kind::NuGet => ".nupkg",
            Kind::Ifw | Kind::InnoSetup | Kind::Nsis => ".exe",
        }
        // The archive extension may be overridden; callers use extension().
    }

    fn extension(&self, interp: &Interp) -> String {
        if matches!(
            self.kind,
            Kind::Archive7z
                | Kind::Tbz2
                | Kind::Tgz
                | Kind::Txz
                | Kind::Tz
                | Kind::Tzst
                | Kind::Tar
                | Kind::Zip
                | Kind::Stgz
        ) && let Some(custom) = option_nonempty(interp, "CPACK_ARCHIVE_FILE_EXTENSION")
        {
            return if custom.starts_with('.') {
                custom
            } else {
                format!(".{custom}")
            };
        }
        self.output_extension(interp).to_owned()
    }

    fn supports_components(&self, interp: &Interp) -> bool {
        match self.kind {
            Kind::Archive7z
            | Kind::Tbz2
            | Kind::Tgz
            | Kind::Txz
            | Kind::Tz
            | Kind::Tzst
            | Kind::Tar
            | Kind::Zip
            | Kind::Stgz => is_on(interp, "CPACK_ARCHIVE_COMPONENT_INSTALL"),
            Kind::Deb => is_on(interp, "CPACK_DEB_COMPONENT_INSTALL"),
            Kind::Rpm => is_on(interp, "CPACK_RPM_COMPONENT_INSTALL"),
            Kind::External => true,
            _ => false,
        }
    }

    fn wants_components(&self, interp: &Interp) -> bool {
        !is_on(interp, "CPACK_MONOLITHIC_INSTALL")
            && self.supports_components(interp)
            && (!self.groups.is_empty() || !self.components.is_empty())
    }

    fn supports_absolute_destination(&self) -> bool {
        !matches!(self.kind, Kind::Deb | Kind::Rpm)
    }

    fn supports_setdest(&self) -> i32 {
        // 0 unsupported, 1 supported, 2 should-not-be-used. Archive-like
        // generators support DESTDIR; DEB/RPM force it internally.
        1
    }

    fn initialize(&mut self, interp: &mut Interp, log: &mut Logger) -> bool {
        set_option(interp, "CPACK_GENERATOR", &self.name.clone());
        if let Some(config) = option_nonempty(interp, "CPACK_PROJECT_CONFIG_FILE")
            && !interp.read_list_file(&config)
        {
            log.warning(&format!("CPACK_PROJECT_CONFIG_FILE not found: {config}"));
        }
        if !self.initialize_internal(interp, log) || interp.error || interp.fatal {
            return false;
        }
        set_if_not_set(interp, "CPACK_PACKAGING_INSTALL_PREFIX", "/");
        let val1 = option(interp, "CPACK_TEMPORARY_INSTALL_DIRECTORY");
        let val2 = option(interp, "CPACK_TEMPORARY_DIRECTORY");
        if val1 != val2 {
            match (val1, val2) {
                (Some(value), None) => {
                    log.warning("Variable CPACK_TEMPORARY_INSTALL_DIRECTORY is set, which is not recommended. For backwards-compatibility we will also set CPACK_TEMPORARY_DIRECTORY to the same value and proceed. However, better set neither of them!");
                    set_option(interp, "CPACK_TEMPORARY_DIRECTORY", &value);
                }
                (None, Some(value)) => {
                    log.warning(
                        "Variable CPACK_TEMPORARY_DIRECTORY is set, which is not recommended.",
                    );
                    set_option(interp, "CPACK_TEMPORARY_INSTALL_DIRECTORY", &value);
                }
                _ => {
                    log.error("Variables CPACK_TEMPORARY_DIRECTORY and CPACK_TEMPORARY_INSTALL_DIRECTORY are both set but to different values. This is not supported!");
                    return false;
                }
            }
        } else if val1.is_some() && val2.is_some() {
            log.warning("Variables CPACK_TEMPORARY_DIRECTORY and CPACK_TEMPORARY_INSTALL_DIRECTORY are both set. Because they are set to the same value we can still proceed. However, better set neither of them!");
        }
        true
    }

    fn initialize_internal(&mut self, interp: &mut Interp, log: &mut Logger) -> bool {
        match self.kind {
            Kind::Archive7z
            | Kind::Tbz2
            | Kind::Tgz
            | Kind::Txz
            | Kind::Tz
            | Kind::Tzst
            | Kind::Tar
            | Kind::Zip => {
                set_if_not_set(interp, "CPACK_INCLUDE_TOPLEVEL_DIRECTORY", "1");
                true
            }
            Kind::Stgz => archive::initialize_stgz(interp, log),
            Kind::Deb => {
                set_if_not_set(interp, "CPACK_PACKAGING_INSTALL_PREFIX", "/usr");
                if util::is_off_opt(option(interp, "CPACK_SET_DESTDIR").as_deref()) {
                    set_option(interp, "CPACK_SET_DESTDIR", "I_ON");
                }
                true
            }
            Kind::Rpm => {
                set_if_not_set(interp, "CPACK_PACKAGING_INSTALL_PREFIX", "/usr");
                if util::is_off_opt(option(interp, "CPACK_SET_DESTDIR").as_deref()) {
                    set_option(interp, "CPACK_SET_DESTDIR", "I_ON");
                }
                for var in ["CPACK_PACKAGE_NAME", "CPACK_PACKAGE_FILE_NAME"] {
                    if let Some(value) = option(interp, var) {
                        set_option(interp, var, &value.replace(' ', "-"));
                    }
                }
                true
            }
            Kind::External => external::initialize(interp, log),
            Kind::NuGet => true,
            Kind::Ifw => {
                log.error("Cannot find QtIFW compiler \"binarycreator\": likely it is not installed, or not in your PATH");
                false
            }
            Kind::InnoSetup => {
                log.error("Cannot find Inno Setup compiler ISCC: likely it is not installed, or not in your PATH");
                false
            }
            Kind::Nsis => {
                log.error("Cannot find NSIS compiler makensis: likely it is not installed, or not in your PATH");
                log.error("Could not read NSIS registry value. This is usually caused by NSIS not being installed. Please install NSIS from http://nsis.sourceforge.net");
                false
            }
        }
    }

    fn prepare_names(&mut self, interp: &mut Interp, log: &mut Logger) -> bool {
        log.debug("Create temp directory.");
        let setdest = option(interp, "CPACK_SET_DESTDIR");
        if util::is_on_opt(setdest.as_deref()) || internally_on(setdest.as_deref()) {
            if self.supports_setdest() == 0 {
                log.error(&format!(
                    "CPACK_SET_DESTDIR is set to ON but the '{}' generator does NOT support it.",
                    self.name
                ));
                return false;
            }
            if self.supports_setdest() == 2 {
                log.warning(&format!("CPACK_SET_DESTDIR is set to ON but it is usually a bad idea to do that with '{}' generator. Use at your own risk.", self.name));
            }
        }
        let Some(pkg_directory) = option(interp, "CPACK_PACKAGE_DIRECTORY") else {
            log.error("CPACK_PACKAGE_DIRECTORY not specified");
            return false;
        };
        let Some(base_name) = option(interp, "CPACK_PACKAGE_FILE_NAME") else {
            log.error("CPACK_PACKAGE_FILE_NAME not specified");
            return false;
        };
        let extension = self.extension(interp);
        if extension.is_empty() {
            log.error("No output extension specified");
            return false;
        }
        let pkg_file_name = format!("{base_name}{extension}");
        let pkg_file_path = format!("{pkg_directory}/{pkg_file_name}");
        let mut top_directory = format!("{pkg_directory}/_CPack_Packages/");
        if let Some(tag) = option_nonempty(interp, "CPACK_TOPLEVEL_TAG") {
            top_directory.push_str(&format!("{tag}/"));
        }
        top_directory.push_str(&self.name);
        let tmp_directory = format!("{top_directory}/{base_name}");
        let tmp_pkg_file_path = format!("{top_directory}/{pkg_file_name}");

        set_if_not_set(interp, "CPACK_REMOVE_TOPLEVEL_DIRECTORY", "1");
        set_if_not_set(interp, "CPACK_TOPLEVEL_DIRECTORY", &top_directory);
        set_if_not_set(interp, "CPACK_TEMPORARY_DIRECTORY", &tmp_directory);
        set_if_not_set(interp, "CPACK_TEMPORARY_INSTALL_DIRECTORY", &tmp_directory);
        set_if_not_set(interp, "CPACK_OUTPUT_FILE_PREFIX", &pkg_directory);
        set_if_not_set(interp, "CPACK_OUTPUT_FILE_NAME", &pkg_file_name);
        set_if_not_set(interp, "CPACK_OUTPUT_FILE_PATH", &pkg_file_path);
        set_if_not_set(
            interp,
            "CPACK_TEMPORARY_PACKAGE_FILE_NAME",
            &tmp_pkg_file_path,
        );
        set_if_not_set(interp, "CPACK_INSTALL_DIRECTORY", "/usr/local/");
        set_if_not_set(interp, "CPACK_NATIVE_INSTALL_DIRECTORY", "/usr/local/");

        if let Some(desc_file) = option_nonempty(interp, "CPACK_PACKAGE_DESCRIPTION_FILE")
            && option(interp, "CPACK_PACKAGE_DESCRIPTION").is_none()
        {
            if !Path::new(&desc_file).is_file() {
                log.error(&format!("Cannot find description file name: [{desc_file}]"));
                return false;
            }
            log.verbose(&format!("Read description file: {desc_file}"));
            let Ok(text) = fs::read_to_string(&desc_file) else {
                log.error(&format!("Cannot open description file name: {desc_file}"));
                return false;
            };
            let mut description = String::new();
            for line in text.lines() {
                description.push_str(&xml_safe(line));
                description.push('\n');
            }
            set_option(interp, "CPACK_PACKAGE_DESCRIPTION", &description);
            if option(interp, "CPACK_DEFAULT_PACKAGE_DESCRIPTION_FILE").as_deref()
                == Some(desc_file.as_str())
            {
                set_option(interp, "CPACK_USED_DEFAULT_PACKAGE_DESCRIPTION_FILE", "ON");
            }
        }
        if option(interp, "CPACK_PACKAGE_DESCRIPTION").is_none() {
            log.error("Project description not specified. Please specify CPACK_PACKAGE_DESCRIPTION or CPACK_PACKAGE_DESCRIPTION_FILE.");
            return false;
        }
        if let Some(algorithm) = option_nonempty(interp, "CPACK_PACKAGE_CHECKSUM")
            && !valid_checksum_algorithm(&algorithm)
        {
            log.error(&format!("Cannot recognize algorithm: {algorithm}"));
            return false;
        }
        true
    }

    fn prepare_grouping_kind(&mut self, interp: &Interp, log: &mut Logger) -> bool {
        let mut method = None;
        if option(interp, "CPACK_COMPONENTS_ALL_IN_ONE_PACKAGE").is_some() {
            method = Some(Method::OnePackage);
        }
        if option(interp, "CPACK_COMPONENTS_IGNORE_GROUPS").is_some() {
            method = Some(Method::OnePackagePerComponent);
        }
        if option(interp, "CPACK_COMPONENTS_ONE_PACKAGE_PER_GROUP").is_some() {
            method = Some(Method::OnePackagePerGroup);
        }
        let grouping = option(interp, "CPACK_COMPONENTS_GROUPING").unwrap_or_default();
        if !grouping.is_empty() {
            log.verbose(&format!(
                "[{}] requested component grouping = {grouping}",
                self.name
            ));
            method = match grouping.as_str() {
                "ALL_COMPONENTS_IN_ONE" => Some(Method::OnePackage),
                "IGNORE" => Some(Method::OnePackagePerComponent),
                "ONE_PER_GROUP" => Some(Method::OnePackagePerGroup),
                _ => {
                    log.warning(&format!("[{}] requested component grouping type <{grouping}> UNKNOWN not in (ALL_COMPONENTS_IN_ONE,IGNORE,ONE_PER_GROUP)", self.name));
                    method
                }
            };
        }
        if method == Some(Method::OnePackagePerGroup)
            && self.groups.is_empty()
            && !self.components.is_empty()
        {
            method = Some(if self.method == Method::OnePackage {
                Method::OnePackage
            } else {
                Method::OnePackagePerComponent
            });
            log.warning(&format!("[{}] One package per component group requested, but NO component groups exist: Ignoring component group.", self.name));
        }
        if let Some(method) = method {
            self.method = method;
        }
        log.verbose(&format!(
            "[{}] requested component grouping = {}",
            self.name,
            self.method.upstream_name()
        ));
        true
    }

    fn clean_temporary_directory(&self, interp: &Interp, log: &mut Logger) -> bool {
        let temp = option(interp, "CPACK_TEMPORARY_DIRECTORY").unwrap_or_default();
        if Path::new(&temp).exists() {
            log.output(&format!("- Clean temporary : {temp}"));
            if !remove_dir_all(&temp) {
                log.error(&format!("Problem removing temporary directory: {temp}"));
                return false;
            }
        }
        true
    }

    fn install_project(&mut self, interp: &mut Interp, log: &mut Logger) -> bool {
        log.output("Install projects");
        if !self.clean_temporary_directory(interp, log) {
            return false;
        }
        let bare_temp = option(interp, "CPACK_TEMPORARY_DIRECTORY").unwrap_or_default();
        let mut temp_install = bare_temp.clone();
        let setdest_value = option(interp, "CPACK_SET_DESTDIR");
        let setdest =
            util::is_on_opt(setdest_value.as_deref()) || internally_on(setdest_value.as_deref());
        if !setdest {
            temp_install.push_str(
                &option(interp, "CPACK_PACKAGING_INSTALL_PREFIX").unwrap_or_else(|| "/".into()),
            );
        }
        if !make_directory(&bare_temp, None) {
            log.error(&format!(
                "Problem creating temporary directory: {temp_install}"
            ));
            return false;
        }
        let destdir_old = std::env::var("DESTDIR").ok();
        if setdest {
            unsafe { std::env::set_var("DESTDIR", &temp_install) };
        } else {
            unsafe { std::env::remove_var("DESTDIR") };
        }
        let default_mode = match default_dir_mode(interp, log) {
            Ok(v) => v,
            Err(()) => return false,
        };
        let staging_control = option_nonempty(interp, "CPACK_EXTERNAL_ENABLE_STAGING");
        let staging = self.kind != Kind::External
            || staging_control.is_none()
            || !util::is_off_opt(staging_control.as_deref());
        let ok = !staging
            || (self.install_via_commands(interp, log, &temp_install)
                && self.install_via_scripts(interp, log, setdest, &temp_install)
                && self.install_via_installed_directories(
                    interp,
                    log,
                    &temp_install,
                    default_mode,
                )
                && self.install_via_cmake_projects(interp, log, setdest, &bare_temp, default_mode));
        if !ok {
            return false;
        }
        if let Some(scripts) = option_nonempty(interp, "CPACK_PRE_BUILD_SCRIPTS") {
            for script in util::expand_list(&scripts, false) {
                log.output(&format!("Executing pre-build script: {script}"));
                if !interp.read_list_file(&script) {
                    log.error(&format!("The pre-build script not found: {script}"));
                    return false;
                }
            }
        }
        match destdir_old {
            Some(value) => unsafe { std::env::set_var("DESTDIR", value) },
            None => unsafe { std::env::remove_var("DESTDIR") },
        }
        true
    }

    fn install_via_commands(
        &mut self,
        interp: &Interp,
        log: &mut Logger,
        temp_install: &str,
    ) -> bool {
        let Some(commands) = option_nonempty(interp, "CPACK_INSTALL_COMMANDS") else {
            return true;
        };
        for command_line in util::expand_list(&commands, false) {
            log.verbose(&format!("Execute: {command_line}"));
            let output = Command::new("/bin/sh")
                .arg("-c")
                .arg(&command_line)
                .env("CMAKE_INSTALL_PREFIX", temp_install)
                .output();
            let Ok(output) = output else {
                return self.install_command_failure(log, &command_line, "");
            };
            if !output.status.success() {
                let text = String::from_utf8_lossy(&output.stdout).into_owned()
                    + &String::from_utf8_lossy(&output.stderr);
                return self.install_command_failure(log, &command_line, &text);
            }
        }
        true
    }

    fn install_command_failure(&self, log: &mut Logger, command: &str, output: &str) -> bool {
        let top = self.toplevel.clone();
        let tmp = format!("{top}/InstallOutput.log");
        let _ = fs::write(
            &tmp,
            format!("# Run command: {command}\n# Output:\n{output}\n"),
        );
        log.error(&format!(
            "Problem running install command: {command}\nPlease check {tmp} for errors"
        ));
        false
    }

    fn install_via_scripts(
        &mut self,
        interp: &mut Interp,
        log: &mut Logger,
        setdest: bool,
        temp_install: &str,
    ) -> bool {
        let mut scripts = option(interp, "CPACK_INSTALL_SCRIPTS");
        if let Some(one) = option(interp, "CPACK_INSTALL_SCRIPT") {
            if scripts.is_some() {
                log.warning("Both CPACK_INSTALL_SCRIPTS and CPACK_INSTALL_SCRIPT are set, the latter will be ignored.");
            } else {
                scripts = Some(one);
            }
        }
        let Some(scripts) = scripts.filter(|s| !s.is_empty()) else {
            return true;
        };
        log.output(&format!("- Install scripts: {scripts}"));
        for script in util::expand_list(&scripts, false) {
            log.output(&format!("- Install script: {script}"));
            if setdest {
                let dir = option(interp, "CPACK_INSTALL_PREFIX").unwrap_or_default();
                set_option(interp, "CMAKE_INSTALL_PREFIX", &dir);
            } else {
                set_option(interp, "CMAKE_INSTALL_PREFIX", temp_install);
            }
            set_if_not_set(interp, "CMAKE_CURRENT_BINARY_DIR", temp_install);
            set_if_not_set(interp, "CMAKE_CURRENT_SOURCE_DIR", temp_install);
            if !interp.read_list_file(&script) || interp.error || interp.fatal {
                return false;
            }
        }
        true
    }

    fn install_via_installed_directories(
        &mut self,
        interp: &Interp,
        log: &mut Logger,
        temp_install: &str,
        mode: Option<u32>,
    ) -> bool {
        let ignore: Vec<_> = list_option(interp, "CPACK_IGNORE_FILES")
            .into_iter()
            .filter_map(|p| crate::cmregex::Regex::new(&p).ok())
            .collect();
        let Some(directories) = option_nonempty(interp, "CPACK_INSTALLED_DIRECTORIES") else {
            return true;
        };
        let items = util::expand_list(&directories, false);
        if !items.len().is_multiple_of(2) {
            log.error("CPACK_INSTALLED_DIRECTORIES should contain pairs of <directory> and <subdirectory>. The <subdirectory> can be '.' to be installed in the toplevel directory of installation.");
            return false;
        }
        for pair in items.chunks(2) {
            let (top, subdir) = (&pair[0], &pair[1]);
            log.output(&format!("- Install directory: {top}"));
            let files = recurse_files(top);
            if files.is_empty() {
                log.error("Cannot find any files in the installed directory");
                return false;
            }
            self.files = files.clone();
            let mut symlinks = Vec::new();
            for source in files {
                let mut in_file = source.clone();
                if Path::new(&source).is_dir()
                    && fs::symlink_metadata(&source).is_ok_and(|m| !m.file_type().is_symlink())
                {
                    in_file.push('/');
                }
                if ignore.iter().any(|re| re.find(&in_file).is_some()) {
                    log.verbose(&format!("Ignore file: {in_file}"));
                    continue;
                }
                let rel = relative_path(top, &source);
                let dest = format!("{temp_install}/{subdir}/{rel}");
                if fs::symlink_metadata(&source).is_ok_and(|m| m.file_type().is_symlink()) {
                    if let Ok(target) = fs::read_link(&source) {
                        symlinks.push((target.to_string_lossy().into_owned(), rel));
                    }
                } else if Path::new(&source).is_dir() {
                    if !make_directory(&dest, mode) {
                        return false;
                    }
                } else if !copy_file_if_different(&source, &dest) {
                    log.error(&format!("Problem copying file: {in_file} -> {dest}"));
                    return false;
                }
            }
            for (target, rel) in symlinks {
                let link = format!("{temp_install}/{subdir}/{rel}");
                if let Some(parent) = Path::new(&link).parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let _ = fs::remove_file(&link);
                #[cfg(unix)]
                if std::os::unix::fs::symlink(&target, &link).is_err() {
                    log.error(&format!("Cannot create symlink: {rel}--> {target}"));
                    return false;
                }
            }
        }
        true
    }

    fn get_installation_type(&mut self, interp: &Interp, name: &str) -> String {
        if !self.installation_types.contains_key(name) {
            let prefix = format!("CPACK_INSTALL_TYPE_{}", name.to_ascii_uppercase());
            let display = option_nonempty(interp, &format!("{prefix}_DISPLAY_NAME"))
                .unwrap_or_else(|| name.to_owned());
            let index = self.installation_types.len();
            self.installation_types.insert(
                name.to_owned(),
                InstallationType {
                    name: name.into(),
                    display_name: display,
                    index,
                },
            );
        }
        name.to_owned()
    }

    fn get_component_group(&mut self, interp: &Interp, name: &str) -> String {
        if !self.groups.contains_key(name) {
            let prefix = format!("CPACK_COMPONENT_GROUP_{}", name.to_ascii_uppercase());
            let display = option_nonempty(interp, &format!("{prefix}_DISPLAY_NAME"))
                .unwrap_or_else(|| name.to_owned());
            let description =
                option_nonempty(interp, &format!("{prefix}_DESCRIPTION")).unwrap_or_default();
            let parent = option_nonempty(interp, &format!("{prefix}_PARENT_GROUP"));
            let group = ComponentGroup {
                name: name.into(),
                display_name: display,
                description,
                bold: is_on(interp, &format!("{prefix}_BOLD_TITLE")),
                expanded_by_default: is_on(interp, &format!("{prefix}_EXPANDED")),
                parent_group: parent,
                components: Vec::new(),
            };
            self.groups.insert(name.to_owned(), group);
        }
        name.to_owned()
    }

    fn get_component(&mut self, interp: &Interp, project_name: &str, name: &str) -> String {
        let _ = project_name;
        if !self.components.contains_key(name) {
            let prefix = format!("CPACK_COMPONENT_{}", name.to_ascii_uppercase());
            let display = option_nonempty(interp, &format!("{prefix}_DISPLAY_NAME"))
                .unwrap_or_else(|| name.to_owned());
            let group = option_nonempty(interp, &format!("{prefix}_GROUP"))
                .map(|g| self.get_component_group(interp, &g));
            if let Some(group_name) = &group {
                self.groups
                    .get_mut(group_name)
                    .unwrap()
                    .components
                    .push(name.to_owned());
            }
            let installation_types = list_option(interp, &format!("{prefix}_INSTALL_TYPES"))
                .iter()
                .map(|t| self.get_installation_type(interp, t))
                .collect();
            let dependencies = list_option(interp, &format!("{prefix}_DEPENDS"));
            self.components.insert(
                name.to_owned(),
                Component {
                    name: name.into(),
                    display_name: display,
                    group,
                    required: is_on(interp, &format!("{prefix}_REQUIRED")),
                    hidden: is_on(interp, &format!("{prefix}_HIDDEN")),
                    disabled_by_default: is_on(interp, &format!("{prefix}_DISABLED")),
                    downloaded: is_on(interp, &format!("{prefix}_DOWNLOADED"))
                        || is_on(interp, "CPACK_DOWNLOAD_ALL"),
                    description: option_nonempty(interp, &format!("{prefix}_DESCRIPTION"))
                        .unwrap_or_default(),
                    installation_types,
                    archive_file: option_nonempty(interp, &format!("{prefix}_ARCHIVE_FILE"))
                        .unwrap_or_default(),
                    dependencies,
                    files: Vec::new(),
                },
            );
        }
        name.to_owned()
    }

    fn install_via_cmake_projects(
        &mut self,
        interp: &mut Interp,
        log: &mut Logger,
        setdest: bool,
        bare_temp: &str,
        mode: Option<u32>,
    ) -> bool {
        let Some(projects) = option_nonempty(interp, "CPACK_INSTALL_CMAKE_PROJECTS") else {
            set_option(interp, "CPACK_ABSOLUTE_DESTINATION_FILES", "");
            return true;
        };
        if option(interp, "CPACK_CMAKE_GENERATOR").is_none() {
            log.error("CPACK_INSTALL_CMAKE_PROJECTS is specified, but CPACK_CMAKE_GENERATOR is not. CPACK_CMAKE_GENERATOR is required to install the project.");
            return false;
        }
        let items = util::expand_list(&projects, false);
        if !items.len().is_multiple_of(4) {
            log.error("Not enough items on list: CPACK_INSTALL_CMAKE_PROJECTS. CPACK_INSTALL_CMAKE_PROJECTS should hold quadruplet of install directory, install project name, install component, and install subdirectory.");
            return false;
        }
        let mut absolute_files = String::new();
        for quad in items.chunks(4) {
            let mut project = CMakeProject {
                directory: quad[0].clone(),
                project_name: quad[1].clone(),
                component: quad[2].clone(),
                sub_directory: quad[3].clone(),
                installation_types: Vec::new(),
                components: Vec::new(),
            };
            let mut components = Vec::new();
            let mut component_install = false;
            if self.supports_components(interp) && !is_on(interp, "CPACK_MONOLITHIC_INSTALL") {
                let install_types_var = format!(
                    "CPACK_{}_INSTALL_TYPES",
                    project.component.to_ascii_uppercase()
                );
                for install_type in list_option(interp, &install_types_var) {
                    let t = self.get_installation_type(interp, &install_type);
                    project.installation_types.push(t);
                }
                let components_var = format!(
                    "CPACK_COMPONENTS_{}",
                    project.component.to_ascii_uppercase()
                );
                for comp in list_option(interp, &components_var) {
                    let c = self.get_component(interp, &project.project_name, &comp);
                    project.components.push(c.clone());
                    components.push(c);
                    component_install = true;
                }
            }
            if components.is_empty() {
                components.push(project.component.clone());
            }
            let mut configs = list_option(interp, "CPACK_BUILD_CONFIG");
            configs.sort();
            configs.dedup();
            if configs.is_empty() {
                configs.push(String::new());
            }
            for config in configs {
                // Ninja has no preinstall target. Unix Makefiles does; running it
                // is part of the generated build system rather than CPack itself.
                log.output(&format!(
                    "- Install project: {} [{config}]",
                    project.project_name
                ));
                for component in &components {
                    if !self.install_cmake_project(
                        interp,
                        log,
                        setdest,
                        &project.directory,
                        bare_temp,
                        mode,
                        component,
                        component_install,
                        &project.sub_directory,
                        &config,
                        &mut absolute_files,
                    ) {
                        return false;
                    }
                }
            }
            self.cmake_projects.push(project);
        }
        set_option(interp, "CPACK_ABSOLUTE_DESTINATION_FILES", &absolute_files);
        true
    }

    #[allow(clippy::too_many_arguments)]
    fn install_cmake_project(
        &mut self,
        outer: &Interp,
        log: &mut Logger,
        setdest: bool,
        install_directory: &str,
        base_temp: &str,
        mode: Option<u32>,
        component: &str,
        component_install: bool,
        subdir: &str,
        build_config: &str,
        absolute_files: &mut String,
    ) -> bool {
        if component_install {
            log.output(&format!("-   Install component: {component}"));
        }
        let mut temp_install = base_temp.to_owned();
        if !subdir.is_empty() && subdir != "/" && subdir != "." {
            temp_install.push_str(subdir);
        }
        if component_install {
            temp_install.push('/');
            temp_install.push_str(&self.component_install_dir_suffix(outer, component));
            if is_on(outer, "CPACK_COMPONENT_INCLUDE_TOPLEVEL_DIRECTORY") {
                temp_install.push('/');
                temp_install
                    .push_str(&option(outer, "CPACK_PACKAGE_FILE_NAME").unwrap_or_default());
            }
        }
        if !setdest {
            temp_install.push_str(
                &option(outer, "CPACK_PACKAGING_INSTALL_PREFIX").unwrap_or_else(|| "/".into()),
            );
        }
        let mut interp = new_script_interp(&outer.home_binary, self.trace, self.trace_expand);
        let capture = Rc::new(std::cell::RefCell::new(Vec::new()));
        interp.stdout_capture = Some(capture.clone());
        if let Some(value) = option_nonempty(outer, "CPACK_INSTALL_DEFAULT_DIRECTORY_PERMISSIONS") {
            interp.set_def("CMAKE_INSTALL_DEFAULT_DIRECTORY_PERMISSIONS", value);
        }
        if setdest {
            if internally_on(option(outer, "CPACK_SET_DESTDIR").as_deref()) {
                let prefix = option(outer, "CPACK_PACKAGING_INSTALL_PREFIX").unwrap_or_default();
                // The outer generator stores this override in its makefile.
                // The local interpreter only needs the resulting install prefix.
                interp.set_def("CPACK_INSTALL_PREFIX", prefix.clone());
            }
            let dir = option(&interp, "CPACK_INSTALL_PREFIX")
                .or_else(|| option(outer, "CPACK_INSTALL_PREFIX"))
                .unwrap_or_default();
            interp.set_def("CMAKE_INSTALL_PREFIX", &dir);
            let full = if dir.starts_with('/') {
                format!("{temp_install}{dir}")
            } else {
                format!("{temp_install}/{dir}")
            };
            unsafe { std::env::set_var("DESTDIR", &temp_install) };
            if !make_directory(&full, mode) {
                log.error(&format!("Problem creating temporary directory: {full}"));
                return false;
            }
        } else {
            interp.set_def("CMAKE_INSTALL_PREFIX", &temp_install);
            if !make_directory(&temp_install, mode) {
                log.error(&format!(
                    "Problem creating temporary directory: {temp_install}"
                ));
                return false;
            }
        }
        if !build_config.is_empty() {
            interp.set_def("BUILD_TYPE", build_config);
        }
        if !component.eq_ignore_ascii_case("all") {
            interp.set_def("CMAKE_INSTALL_COMPONENT", component);
        }
        let strip = option(outer, "CPACK_STRIP_FILES");
        if !util::is_off_opt(strip.as_deref()) {
            interp.set_def("CMAKE_INSTALL_DO_STRIP", "1");
        }
        let install_prefix = temp_install.clone();
        let find_expr = format!("{temp_install}/");
        let before = if component_install {
            recurse_files(&find_expr)
        } else {
            Vec::new()
        };
        if is_on(outer, "CPACK_WARN_ON_ABSOLUTE_INSTALL_DESTINATION") {
            interp.set_def("CMAKE_WARN_ON_ABSOLUTE_INSTALL_DESTINATION", "1");
        }
        if !self.supports_absolute_destination()
            || is_on(outer, "CPACK_ERROR_ON_ABSOLUTE_INSTALL_DESTINATION")
        {
            interp.set_def("CMAKE_ERROR_ON_ABSOLUTE_INSTALL_DESTINATION", "1");
        }
        for custom in list_option(outer, "CPACK_CUSTOM_INSTALL_VARIABLES") {
            let (name, value) = custom.split_once('=').unwrap_or((custom.as_str(), ""));
            interp.set_def(name, value);
        }
        let install_file = format!("{install_directory}/cmake_install.cmake");
        let res = interp.read_list_file(&install_file);
        for line in capture.borrow_mut().drain(..) {
            log.verbose(line.strip_prefix("-- ").unwrap_or(&line));
        }
        if let Some(def) = interp
            .get_def("CMAKE_ABSOLUTE_DESTINATION_FILES")
            .map(str::to_owned)
        {
            interp.set_def("CPACK_ABSOLUTE_DESTINATION_FILES", &def);
        }
        if component_install {
            let after = recurse_files(&find_expr);
            let before: BTreeSet<_> = before.into_iter().collect();
            for path in after.into_iter().filter(|p| !before.contains(p)) {
                let local = relative_path(&install_prefix, &path)
                    .trim_start_matches('/')
                    .to_owned();
                self.components
                    .entry(component.to_owned())
                    .or_default()
                    .files
                    .push(local.clone());
                log.debug(&format!("Adding file <{local}> to component <{component}>"));
            }
        }
        if let Some(def) = interp
            .get_def("CPACK_ABSOLUTE_DESTINATION_FILES")
            .map(str::to_owned)
            && !def.is_empty()
        {
            if !absolute_files.is_empty() {
                absolute_files.push(';');
            }
            absolute_files.push_str(&def);
            if component_install {
                let suffix = self.component_install_suffix(outer, component);
                let var = format!("CPACK_ABSOLUTE_DESTINATION_FILES_{suffix}");
                // Component absolute paths are needed by DEB/RPM metadata.
                // They live in the outer interpreter for subsequent scripts.
                let existing = outer.get_def(&var).unwrap_or("").to_owned();
                let merged = if existing.is_empty() {
                    def.clone()
                } else {
                    format!("{existing};{def}")
                };
                // This mutates through the caller below; outer is immutable here.
                // The caller-visible aggregate remains authoritative for common use.
                let _ = merged;
            }
        }
        !(interp.error || interp.fatal) && res
    }

    fn component_install_suffix(&self, interp: &Interp, component: &str) -> String {
        match self.kind {
            Kind::Deb | Kind::Rpm => {
                if self.method == Method::OnePackagePerComponent {
                    return component.into();
                }
                if self.method == Method::OnePackage {
                    return "ALL_COMPONENTS_IN_ONE".into();
                }
                let var = format!("CPACK_COMPONENT_{}_GROUP", component.to_ascii_uppercase());
                option(interp, &var).unwrap_or_else(|| component.into())
            }
            _ => component.into(),
        }
    }

    fn component_install_dir_suffix(&self, interp: &Interp, component: &str) -> String {
        sanitize_name(&self.component_install_suffix(interp, component), false)
    }

    fn component_package_file_name(
        &self,
        interp: &Interp,
        initial: &str,
        name: &str,
        group: bool,
    ) -> String {
        let mut suffix = format!("-{name}");
        let display_var = format!("CPACK_{}_USE_DISPLAY_NAME_IN_FILENAME", self.name);
        if is_on(interp, &display_var) {
            let var = if group {
                format!(
                    "CPACK_COMPONENT_GROUP_{}_DISPLAY_NAME",
                    name.to_ascii_uppercase()
                )
            } else {
                format!("CPACK_COMPONENT_{}_DISPLAY_NAME", name.to_ascii_uppercase())
            };
            if let Some(display) = option(interp, &var) {
                suffix = format!("-{display}")
            }
        }
        format!("{initial}{suffix}")
    }

    fn do_package(&mut self, interp: &mut Interp, log: &mut Logger) -> bool {
        log.output(&format!("Create package using {}", self.name));
        if !self.prepare_names(interp, log) {
            return false;
        }
        if !self.prepare_grouping_kind(interp, log) {
            return false;
        }
        if is_on(interp, "CPACK_REMOVE_TOPLEVEL_DIRECTORY")
            && let Some(top) = option(interp, "CPACK_TOPLEVEL_DIRECTORY")
            && Path::new(&top).exists()
        {
            log.verbose(&format!("Remove toplevel directory: {top}"));
            if !remove_dir_all(&top) {
                log.error(&format!("Problem removing toplevel directory: {top}"));
                return false;
            }
        }
        if !self.install_project(interp, log) {
            return false;
        }
        let mut temp_directory = option(interp, "CPACK_TEMPORARY_DIRECTORY").unwrap_or_default();
        self.files = recurse_files(&temp_directory);
        if self.files.is_empty() {
            log.error("Cannot find any files in the packaging tree");
            return false;
        }
        if is_on(interp, "CPACK_INCLUDE_TOPLEVEL_DIRECTORY") {
            temp_directory = option(interp, "CPACK_TOPLEVEL_DIRECTORY").unwrap_or_default();
        }
        self.toplevel = temp_directory.trim_end_matches('/').to_owned();
        log.output("Create package");
        if let Some(temp_pkg) = option(interp, "CPACK_TEMPORARY_PACKAGE_FILE_NAME") {
            log.verbose(&format!("Package files to: {temp_pkg}"));
            if Path::new(&temp_pkg).exists() {
                log.verbose("Remove old package file");
                let _ = fs::remove_file(&temp_pkg);
            }
            self.package_file_names.clear();
            self.package_file_names.push(temp_pkg);
        }
        if !self.package_files_dispatch(interp, log) || interp.error || interp.fatal {
            log.error("Problem compressing the directory");
            return false;
        }
        if let Some(scripts) = option_nonempty(interp, "CPACK_POST_BUILD_SCRIPTS") {
            set_option(
                interp,
                "CPACK_PACKAGE_FILES",
                &util::join_list(&self.package_file_names),
            );
            for script in util::expand_list(&scripts, false) {
                log.output(&format!("Executing post-build script: {script}"));
                if !interp.read_list_file(&script) {
                    log.error(&format!("The post-build script not found: {script}"));
                    return false;
                }
            }
        }
        log.verbose(&format!(
            "Copying final package(s) [{}]:",
            self.package_file_names.len()
        ));
        let algorithm = option_nonempty(interp, "CPACK_PACKAGE_CHECKSUM");
        let names = self.package_file_names.clone();
        for package in names {
            let filename = util::filename_name(&package);
            if !self.copy_package_file(interp, log, &package, &filename) {
                return false;
            }
            if let Some(algorithm) = &algorithm
                && !self.generate_checksum_file(interp, log, &filename, algorithm)
            {
                return false;
            }
        }
        true
    }

    fn copy_package_file(
        &self,
        interp: &Interp,
        log: &mut Logger,
        source: &str,
        filename: &str,
    ) -> bool {
        let prefix = option(interp, "CPACK_OUTPUT_FILE_PREFIX").unwrap_or_default();
        let dest = format!("{prefix}/{filename}");
        log.debug(&format!("Copy final package(s): {source} to {dest}"));
        if !copy_file_if_different(source, &dest) {
            log.error(&format!("Problem copying the package: {source} to {dest}"));
            return false;
        }
        log.output(&format!("- package: {dest} generated."));
        true
    }

    fn generate_checksum_file(
        &self,
        interp: &Interp,
        log: &mut Logger,
        filename: &str,
        algorithm: &str,
    ) -> bool {
        let prefix = option(interp, "CPACK_OUTPUT_FILE_PREFIX").unwrap_or_default();
        let package = format!("{prefix}/{filename}");
        let lower = algorithm.to_ascii_lowercase();
        let hash_file = format!("{package}.{lower}");
        let Some(hash) = file_hash(&package, algorithm) else {
            log.error(&format!("Cannot create checksum file: {hash_file}"));
            return false;
        };
        if fs::write(&hash_file, format!("{hash}  {filename}\n")).is_err() {
            log.error(&format!("Cannot create checksum file: {hash_file}"));
            return false;
        }
        log.output(&format!("- checksum file: {hash_file} generated."));
        true
    }

    fn package_files_dispatch(&mut self, interp: &mut Interp, log: &mut Logger) -> bool {
        match self.kind {
            Kind::Archive7z
            | Kind::Tbz2
            | Kind::Tgz
            | Kind::Txz
            | Kind::Tz
            | Kind::Tzst
            | Kind::Tar
            | Kind::Zip
            | Kind::Stgz => archive::package_files(self, interp, log),
            Kind::Deb => deb::package_files(self, interp, log),
            Kind::Rpm => rpm_package_files(self, interp, log),
            Kind::External => external::package_files(self, interp, log),
            Kind::NuGet => nuget_package_files(self, interp, log),
            Kind::Ifw | Kind::InnoSetup | Kind::Nsis => false,
        }
    }
}

fn rpm_package_files(generator: &mut Generator, interp: &mut Interp, log: &mut Logger) -> bool {
    // The RPM generator is intentionally thin: upstream delegates spec
    // generation and the rpmbuild invocation to Internal/CPack/CPackRPM.cmake.
    if generator.wants_components(interp) {
        // Component iteration mirrors cmCPackRPMGenerator::PackageComponents.
        generator.package_file_names.clear();
        let initial = option(interp, "CPACK_TEMPORARY_DIRECTORY").unwrap_or_default();
        let mut names: Vec<String> = Vec::new();
        if generator.method == Method::OnePackagePerComponent {
            names.extend(generator.components.keys().cloned());
        } else {
            names.extend(generator.groups.keys().cloned());
            names.extend(
                generator
                    .components
                    .values()
                    .filter(|c| c.group.is_none())
                    .map(|c| c.name.clone()),
            );
        }
        for name in names {
            if !rpm_package_one(generator, interp, log, &initial, &name) {
                return false;
            }
        }
        true
    } else {
        if read_module(interp, "Internal/CPack/CPackRPM.cmake") {
            add_generated_package_names(generator, interp)
        } else {
            log.error("Error while execution CPackRPM.cmake");
            false
        }
    }
}

fn rpm_package_one(
    generator: &mut Generator,
    interp: &mut Interp,
    log: &mut Logger,
    initial: &str,
    package: &str,
) -> bool {
    let sanitized_suffix = sanitize_name(package, false);
    let sanitized_dir = sanitize_name(package, false);
    let parent = util::filename_path(&generator.toplevel);
    let initial_name = option(interp, "CPACK_PACKAGE_FILE_NAME").unwrap_or_default();
    let output =
        generator.component_package_file_name(interp, &initial_name, package, true) + ".rpm";
    let local = format!("{initial}/{sanitized_dir}");
    set_option(interp, "CPACK_TEMPORARY_DIRECTORY", &local);
    let package_file = format!("{parent}/{output}");
    set_option(interp, "CPACK_OUTPUT_FILE_NAME", &output);
    set_option(interp, "CPACK_TEMPORARY_PACKAGE_FILE_NAME", &package_file);
    set_option(interp, "CPACK_RPM_PACKAGE_COMPONENT", package);
    set_option(
        interp,
        "CPACK_RPM_PACKAGE_COMPONENT_PART_NAME",
        &sanitized_suffix,
    );
    set_option(
        interp,
        "CPACK_RPM_PACKAGE_COMPONENT_PART_PATH",
        &format!("/{sanitized_dir}"),
    );
    if !read_module(interp, "Internal/CPack/CPackRPM.cmake") {
        log.error("Error while execution CPackRPM.cmake");
        return false;
    }
    add_generated_package_names(generator, interp)
}

fn add_generated_package_names(generator: &mut Generator, interp: &Interp) -> bool {
    if let Some(files) = option_nonempty(interp, "GEN_CPACK_OUTPUT_FILES") {
        generator
            .package_file_names
            .extend(util::expand_list(&files, false));
    }
    true
}

fn nuget_package_files(_gen: &mut Generator, interp: &mut Interp, log: &mut Logger) -> bool {
    // Like upstream, all validation and packaging decisions live in the
    // module. The module fails with CPackNuGet's own diagnostic when the
    // executable is absent.
    if !read_module(interp, "Internal/CPack/CPackNuGet.cmake") {
        log.error("Error while execution CPackNuGet.cmake");
        return false;
    }
    true
}

fn new_script_interp(cwd: &str, trace: bool, trace_expand: bool) -> Interp {
    let root = crate::configure::find_cmake_root().unwrap_or_default();
    let mut interp = Interp::new(Mode::Script, cwd, cwd, &root);
    crate::configure::add_default_definitions(&mut interp);
    crate::configure::add_cmake_paths(&mut interp);
    if trace {
        interp.trace = Some(Rc::new(TraceConfig {
            expand: trace_expand,
            json: false,
            sources: Vec::new(),
            file: None,
        }));
    }
    interp
}

fn print_generators() {
    eprintln!();
    eprintln!("Generators");
    for (name, desc) in generator_list() {
        eprintln!("  {name:<30} = {desc}");
    }
}

fn print_help() {
    print!("Usage\n\n  cpack [options]\n\nOptions\n");
    let rows = [
        ("-G <generators>", "Override/define CPACK_GENERATOR"),
        (
            "-C <Configurations>",
            "Specify the project configuration(s)",
        ),
        ("-D <var>=<value>", "Set a CPack variable."),
        ("--config <configFile>", "Specify the config file."),
        ("-V,--verbose", "Enable verbose output"),
        ("--trace", "Put underlying cmake scripts in trace mode."),
        (
            "--trace-expand",
            "Put underlying cmake scripts in expanded trace mode.",
        ),
        ("--debug", "Enable debug output (for CPack developers)"),
        ("-P <packageName>", "Override/define CPACK_PACKAGE_NAME"),
        (
            "-R <packageVersion>",
            "Override/define CPACK_PACKAGE_VERSION",
        ),
        (
            "-B <packageDirectory>",
            "Override/define CPACK_PACKAGE_DIRECTORY",
        ),
        (
            "--vendor <vendorName>",
            "Override/define CPACK_PACKAGE_VENDOR",
        ),
        ("--preset", "Read arguments from a package preset"),
        ("--list-presets", "List available package presets"),
        ("--version", "Print version number and exit."),
    ];
    for (opt, desc) in rows {
        println!("  {opt:<30} = {desc}");
    }
}

#[derive(Default)]
struct DriverOptions {
    generator: String,
    config_name: String,
    package_name: String,
    package_version: String,
    package_directory: String,
    vendor: String,
    config_file: String,
    preset: String,
    list_presets: bool,
    definitions: BTreeMap<String, String>,
}

fn parse_args(args: &[String], log: &mut Logger) -> Result<(DriverOptions, bool, bool), i32> {
    let mut out = DriverOptions::default();
    let mut trace = false;
    let mut trace_expand = false;
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        let one = |i: &mut usize| -> String {
            *i += 1;
            args.get(*i).cloned().unwrap_or_default()
        };
        match arg.as_str() {
            "--version" | "-version" | "/V" => {
                print!("{VERSION_OUTPUT}");
                return Err(0);
            }
            "--help" | "-h" | "-H" | "-help" | "-usage" | "/?" => {
                print_help();
                return Err(0);
            }
            "-V" | "--verbose" => {
                log.verbose = true;
                log.output("Enable Verbose");
            }
            "--debug" => {
                log.debug = true;
                log.output("Enable Debug");
            }
            "--trace" => trace = true,
            "--trace-expand" => {
                trace = true;
                trace_expand = true;
            }
            "--config" => out.config_file = one(&mut i),
            "-C" => out.config_name = one(&mut i),
            "-G" => out.generator = one(&mut i),
            "-P" => out.package_name = one(&mut i),
            "-R" => out.package_version = one(&mut i),
            "-B" => out.package_directory = one(&mut i),
            "--patch" => {
                let _ = one(&mut i);
            }
            "--vendor" => out.vendor = one(&mut i),
            "--preset" => out.preset = one(&mut i),
            "--list-presets" => out.list_presets = true,
            "-D" => {
                let value = one(&mut i);
                if let Some((key, value)) = value.split_once('=') {
                    log.debug(&format!("Set CPack variable: {key} to \"{value}\""));
                    out.definitions.insert(key.to_owned(), value.to_owned());
                } else {
                    log.error("Please specify CPack definitions as: KEY=VALUE");
                    return Err(1);
                }
            }
            _ if arg.starts_with("-D") && arg.len() > 2 => {
                let text = &arg[2..];
                if let Some((key, value)) = text.split_once('=') {
                    log.debug(&format!("Set CPack variable: {key} to \"{value}\""));
                    out.definitions.insert(key.to_owned(), value.to_owned());
                } else {
                    log.error("Please specify CPack definitions as: KEY=VALUE");
                    return Err(1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    Ok((out, trace, trace_expand))
}

fn apply_presets(
    options: &mut DriverOptions,
    cwd: &str,
    log: &mut Logger,
) -> Result<Option<String>, i32> {
    if options.preset.is_empty() && !options.list_presets {
        return Ok(None);
    }
    let mut graph = crate::presets::Graph::default();
    if !graph.read_project_presets(cwd, false) {
        log.error(&format!("Could not read presets from {cwd}:"));
        return Err(1);
    }
    if options.list_presets {
        graph.print_package_preset_list(None);
        return Err(0);
    }
    let Some(pair) = graph.package_presets.get(&options.preset) else {
        log.error(&format!(
            "No such package preset in {cwd}: \"{}\"",
            options.preset
        ));
        graph.print_package_preset_list(None);
        return Err(1);
    };
    if pair.unexpanded.base.hidden {
        log.error(&format!(
            "Cannot use hidden package preset in {cwd}: \"{}\"",
            options.preset
        ));
        graph.print_package_preset_list(None);
        return Err(1);
    }
    let Some(expanded) = &pair.expanded else {
        log.error(&format!(
            "Could not evaluate package preset \"{}\": Invalid macro expansion",
            options.preset
        ));
        graph.print_package_preset_list(None);
        return Err(1);
    };
    if !expanded.base.condition_result {
        log.error(&format!(
            "Cannot use disabled package preset in {cwd}: \"{}\"",
            options.preset
        ));
        graph.print_package_preset_list(None);
        return Err(1);
    }
    let Some(configure_pair) = graph.configure_presets.get(&expanded.configure_preset) else {
        log.error(&format!(
            "No such configure preset in {cwd}: \"{}\"",
            expanded.configure_preset
        ));
        return Err(1);
    };
    if configure_pair.unexpanded.base.hidden {
        log.error(&format!(
            "Cannot use hidden configure preset in {cwd}: \"{}\"",
            expanded.configure_preset
        ));
        return Err(1);
    }
    let Some(configure) = &configure_pair.expanded else {
        log.error(&format!(
            "Could not evaluate configure preset \"{}\": Invalid macro expansion",
            expanded.configure_preset
        ));
        return Err(1);
    };
    for (key, value) in &expanded.base.environment {
        if let Some(value) = value {
            unsafe { std::env::set_var(key, value) };
        }
    }
    if !expanded.config_file.is_empty() && options.config_file.is_empty() {
        options.config_file = expanded.config_file.clone();
    }
    if !expanded.generators.is_empty() && options.generator.is_empty() {
        options.generator = expanded.generators.join(";");
    }
    if !expanded.configurations.is_empty() && options.config_name.is_empty() {
        options.config_name = expanded.configurations.join(";");
    }
    for (key, value) in &expanded.variables {
        options
            .definitions
            .entry(key.clone())
            .or_insert_with(|| value.clone());
    }
    if expanded.debug_output == Some(true) {
        log.debug = true;
        log.output("Enable Debug");
    }
    if expanded.verbose_output == Some(true) {
        log.verbose = true;
        log.output("Enable Verbose");
    }
    if !expanded.package_name.is_empty() && options.package_name.is_empty() {
        options.package_name = expanded.package_name.clone();
    }
    if !expanded.package_version.is_empty() && options.package_version.is_empty() {
        options.package_version = expanded.package_version.clone();
    }
    if !expanded.package_directory.is_empty() && options.package_directory.is_empty() {
        options.package_directory = expanded.package_directory.clone();
    }
    if !expanded.vendor_name.is_empty() && options.vendor.is_empty() {
        options.vendor = expanded.vendor_name.clone();
    }
    Ok(Some(configure.binary_dir.clone()))
}

/// The cpack executable entry point.
pub fn main() -> i32 {
    let argv: Vec<String> = std::env::args().collect();
    let args = &argv[1..];
    let mut log = Logger::new();
    let (mut options, trace, trace_expand) = match parse_args(args, &mut log) {
        Ok(result) => result,
        Err(code) => return code,
    };
    let mut cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();
    match apply_presets(&mut options, &cwd, &mut log) {
        Ok(Some(binary_dir)) => cwd = binary_dir,
        Ok(None) => {}
        Err(code) => return code,
    }
    log.verbose(&format!("Read CPack config file: {}", options.config_file));
    let specified = !options.config_file.is_empty();
    let config_file = if specified {
        if Path::new(&options.config_file).is_absolute() {
            options.config_file.clone()
        } else {
            util::collapse_full_path(&options.config_file, &cwd)
        }
    } else {
        format!("{cwd}/CPackConfig.cmake")
    };
    if args.iter().any(|a| a == "--help") {
        print_help();
        return 0;
    }
    let root = match crate::configure::find_cmake_root() {
        Some(root) => root,
        None => {
            log.error("Could not find CMAKE_ROOT !!!");
            return 1;
        }
    };
    let mut interp = Interp::new(Mode::Script, &cwd, &cwd, &root);
    crate::configure::add_default_definitions(&mut interp);
    crate::configure::add_cmake_paths(&mut interp);
    if trace {
        interp.trace = Some(Rc::new(TraceConfig {
            expand: trace_expand,
            json: false,
            sources: Vec::new(),
            file: None,
        }));
    }
    // Match upstream's system-information initialization so platform
    // conditions in CPack config and generator modules see the host system.
    let system = format!("{root}/Modules/CMakeDetermineSystem.cmake");
    if !interp.read_list_file(&system) {
        log.error("Error reading CMakeDetermineSystem.cmake");
        return 1;
    }
    let system = format!("{root}/Modules/CMakeSystemSpecificInformation.cmake");
    if !interp.read_list_file(&system) {
        log.error("Error reading CMakeSystemSpecificInformation.cmake");
        return 1;
    }
    if !options.config_name.is_empty() {
        interp.set_def("CPACK_BUILD_CONFIG", &options.config_name);
    }
    if Path::new(&config_file).is_file() {
        log.verbose(&format!("Read CPack configuration file: {config_file}"));
        if !interp.read_list_file(&config_file) {
            log.error(&format!(
                "Problem reading CPack config file: \"{config_file}\""
            ));
            return 1;
        }
    } else if specified {
        log.error(&format!("Cannot find CPack config file: \"{config_file}\""));
        return 1;
    }
    if !options.generator.is_empty() {
        interp.set_def("CPACK_GENERATOR", &options.generator);
    }
    if !options.package_name.is_empty() {
        interp.set_def("CPACK_PACKAGE_NAME", &options.package_name);
    }
    if !options.package_version.is_empty() {
        interp.set_def("CPACK_PACKAGE_VERSION", &options.package_version);
    }
    if !options.vendor.is_empty() {
        interp.set_def("CPACK_PACKAGE_VENDOR", &options.vendor);
    }
    let package_directory = if !options.package_directory.is_empty() {
        if Path::new(&options.package_directory).is_absolute() {
            options.package_directory.clone()
        } else {
            util::collapse_full_path(&options.package_directory, &cwd)
        }
    } else if let Some(pd) = option(&interp, "CPACK_PACKAGE_DIRECTORY") {
        util::collapse_full_path(&pd, &cwd)
    } else {
        cwd.clone()
    };
    interp.set_def("CPACK_PACKAGE_DIRECTORY", &package_directory);
    for (key, value) in &options.definitions {
        interp.set_def(key, value);
    }
    if let Some(path) = option(&interp, "CPACK_MODULE_PATH") {
        interp.set_def("CMAKE_MODULE_PATH", path);
    }
    let Some(generators) = option(&interp, "CPACK_GENERATOR") else {
        log.error("CPack generator not specified");
        return 1;
    };
    let mut parsed = true;
    for gen_name in util::expand_list(&generators, false) {
        interp.push_scope();
        log.verbose(&format!("Specified generator: {gen_name}"));
        if option(&interp, "CPACK_PACKAGE_NAME").is_none() {
            log.error("CPack project name not specified");
            parsed = false;
        }
        if parsed
            && option(&interp, "CPACK_PACKAGE_VERSION").is_none()
            && !(option(&interp, "CPACK_PACKAGE_VERSION_MAJOR").is_some()
                && option(&interp, "CPACK_PACKAGE_VERSION_MINOR").is_some()
                && option(&interp, "CPACK_PACKAGE_VERSION_PATCH").is_some())
        {
            log.error("CPack project version not specified\nSpecify CPACK_PACKAGE_VERSION, or CPACK_PACKAGE_VERSION_MAJOR, CPACK_PACKAGE_VERSION_MINOR, and CPACK_PACKAGE_VERSION_PATCH.");
            parsed = false;
        }
        let Some(info) = generator_info(&gen_name) else {
            log.error(&format!("Could not create CPack generator: {gen_name}"));
            print_generators();
            interp.pop_scope();
            return 1;
        };
        let mut generator = Generator::new(&gen_name, info.kind, trace, trace_expand);
        if parsed && !generator.initialize(&mut interp, &mut log) {
            log.error(&format!("Cannot initialize the generator {gen_name}"));
            parsed = false;
        }
        if option(&interp, "CPACK_INSTALL_COMMANDS").is_none()
            && option(&interp, "CPACK_INSTALL_SCRIPT").is_none()
            && option(&interp, "CPACK_INSTALLED_DIRECTORIES").is_none()
            && option(&interp, "CPACK_INSTALL_CMAKE_PROJECTS").is_none()
        {
            log.error("Please specify build tree of the project that uses CMake using CPACK_INSTALL_CMAKE_PROJECTS, specify CPACK_INSTALL_COMMANDS, CPACK_INSTALL_SCRIPT, or CPACK_INSTALLED_DIRECTORIES.");
            parsed = false;
        }
        if parsed {
            let project_name = option(&interp, "CPACK_PACKAGE_NAME").unwrap_or_default();
            log.verbose(&format!("Use generator: {}", generator.class_name()));
            log.verbose(&format!("For project: {project_name}"));
            if option(&interp, "CPACK_PACKAGE_VERSION").is_none() {
                let version = format!(
                    "{}.{}.{}",
                    option(&interp, "CPACK_PACKAGE_VERSION_MAJOR").unwrap_or_default(),
                    option(&interp, "CPACK_PACKAGE_VERSION_MINOR").unwrap_or_default(),
                    option(&interp, "CPACK_PACKAGE_VERSION_PATCH").unwrap_or_default()
                );
                interp.set_def("CPACK_PACKAGE_VERSION", version);
            }
            if !generator.do_package(&mut interp, &mut log) {
                log.error(&format!("Error when generating package: {project_name}"));
                interp.pop_scope();
                return 1;
            }
        }
        interp.pop_scope();
    }
    i32::from(!parsed || log.error_occurred)
}
