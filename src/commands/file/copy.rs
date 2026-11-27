//! cmFileCopier and cmFileInstaller.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::time::Duration;

use crate::cmregex::Regex;
use crate::interp::{CmdErr, CmdResult, Interp, err};
use crate::util::{expand_list, filename_name, filename_path, is_full_path, is_on};

use super::super::{legacy_fs::collapse_from_cwd, path::relative};
use super::glob::pattern_regex;
use super::{
    copy_file_always, copy_times, mkdir, mode, same_file, source_path, system_error, unix_slashes,
};

#[derive(Default)]
struct MatchProperties {
    exclude: bool,
    mode: u32,
}

struct Rule {
    regex: Regex,
    properties: MatchProperties,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum InstallType {
    #[default]
    File,
    Program,
    Executable,
    StaticLibrary,
    SharedLibrary,
    Module,
    Directory,
}

#[derive(Clone, Copy, Default)]
enum InstallMode {
    #[default]
    Copy,
    Link {
        try_relative: bool,
        require_relative: bool,
        or_copy: bool,
    },
}

#[derive(Default)]
struct Installer {
    kind: InstallType,
    mode: InstallMode,
    rename: String,
    optional: bool,
    message_always: bool,
    message_lazy: bool,
    message_never: bool,
    destdir_len: usize,
    manifest: String,
}

#[derive(Default)]
struct Copier {
    files: Vec<String>,
    destination: String,
    files_from_dir: String,
    source_permissions: bool,
    file_permissions: Option<u32>,
    directory_permissions: Option<u32>,
    matching_only: bool,
    follow_chain: bool,
    always: bool,
    rules: Vec<Rule>,
    installer: Option<Installer>,
}

#[derive(Clone, Copy)]
enum Argument {
    Files,
    Destination,
    FilesFromDir,
    Pattern,
    Regex,
    MatchPermissions,
    FilePermissions,
    DirectoryPermissions,
    InstallType,
    Rename,
    None,
}

pub(super) fn run(interp: &mut Interp, args: &[String]) -> CmdResult {
    let installing = args[0] == "INSTALL";
    let mut copier = Copier {
        source_permissions: !installing,
        always: installing
            && std::env::var("CMAKE_INSTALL_ALWAYS").is_ok_and(|value| is_on(&value)),
        installer: installing.then(|| Installer {
            manifest: interp
                .get_def("CMAKE_INSTALL_MANIFEST_FILES")
                .unwrap_or_default()
                .to_owned(),
            ..Installer::default()
        }),
        ..Copier::default()
    };
    let result = copier
        .parse(interp, &args[1..])
        .and_then(|()| copier.run(interp));
    if let Some(installer) = copier.installer {
        interp.set_def("CMAKE_INSTALL_MANIFEST_FILES", installer.manifest);
    }
    result
}

impl Copier {
    fn name(&self) -> &'static str {
        if self.installer.is_some() {
            "INSTALL"
        } else {
            "COPY"
        }
    }

    fn parse(&mut self, interp: &Interp, args: &[String]) -> CmdResult {
        let mut state = Argument::Files;
        for arg in args {
            let mut next = self.keyword(arg)?;
            if next.is_none() {
                next = self.install_keyword(arg)?;
            }
            if let Some(next) = next {
                state = next;
            } else {
                state = self.value(interp, arg, state)?;
            }
        }
        if self.destination.is_empty() {
            return err(format!("{} given no DESTINATION", self.name()));
        }
        if !self.source_permissions {
            let executable = self.installer.as_ref().is_some_and(|installer| {
                matches!(
                    installer.kind,
                    InstallType::Program | InstallType::Executable
                ) || matches!(
                    installer.kind,
                    InstallType::SharedLibrary | InstallType::Module
                ) && !interp.is_on("CMAKE_INSTALL_SO_NO_EXE")
            });
            self.file_permissions
                .get_or_insert(if executable { 0o755 } else { 0o644 });
            self.directory_permissions.get_or_insert(0o755);
        }
        if self.installer.is_some() {
            self.prepare_install(interp)?;
        }
        Ok(())
    }

    fn keyword(&mut self, arg: &str) -> Result<Option<Argument>, CmdErr> {
        let matching = !self.rules.is_empty();
        let next = match arg {
            "PATTERN" => Argument::Pattern,
            "REGEX" => Argument::Regex,
            "FOLLOW_SYMLINK_CHAIN" => {
                self.follow_chain = true;
                Argument::None
            }
            "EXCLUDE" => {
                let rule = self.rules.last_mut().ok_or_else(|| before_match(arg))?;
                rule.properties.exclude = true;
                Argument::None
            }
            "PERMISSIONS" if matching => Argument::MatchPermissions,
            "PERMISSIONS" if self.installer.is_none() => return Err(before_match(arg)),
            "DESTINATION"
            | "FILES_FROM_DIR"
            | "FILE_PERMISSIONS"
            | "DIRECTORY_PERMISSIONS"
            | "USE_SOURCE_PERMISSIONS"
            | "NO_SOURCE_PERMISSIONS"
            | "FILES_MATCHING" => {
                if matching {
                    return Err(after_match(arg));
                }
                match arg {
                    "DESTINATION" => Argument::Destination,
                    "FILES_FROM_DIR" => Argument::FilesFromDir,
                    "FILE_PERMISSIONS" => {
                        self.file_permissions.get_or_insert(0);
                        Argument::FilePermissions
                    }
                    "DIRECTORY_PERMISSIONS" => {
                        self.directory_permissions.get_or_insert(0);
                        Argument::DirectoryPermissions
                    }
                    "FILES_MATCHING" => {
                        self.matching_only = true;
                        Argument::None
                    }
                    _ => {
                        self.source_permissions = arg == "USE_SOURCE_PERMISSIONS";
                        Argument::None
                    }
                }
            }
            _ => return Ok(None),
        };
        Ok(Some(next))
    }

    fn install_keyword(&mut self, arg: &str) -> Result<Option<Argument>, CmdErr> {
        let Some(installer) = &mut self.installer else {
            return Ok(None);
        };
        let next = match arg {
            "COMPONENTS" | "CONFIGURATIONS" | "PROPERTIES" => {
                return Err(CmdErr::Error(format!(
                    "INSTALL called with old-style {arg} argument.  This script was generated with an older version of CMake.  Re-run this cmake version on your build tree."
                )));
            }
            "TYPE" | "FILES" | "RENAME" | "OPTIONAL" | "MESSAGE_ALWAYS" | "MESSAGE_LAZY"
            | "MESSAGE_NEVER" | "PERMISSIONS" | "DIR_PERMISSIONS" => {
                if !self.rules.is_empty() {
                    return Err(after_match(arg));
                }
                match arg {
                    "TYPE" => Argument::InstallType,
                    "FILES" => Argument::Files,
                    "RENAME" => Argument::Rename,
                    "PERMISSIONS" => {
                        self.file_permissions.get_or_insert(0);
                        Argument::FilePermissions
                    }
                    "DIR_PERMISSIONS" => {
                        self.directory_permissions.get_or_insert(0);
                        Argument::DirectoryPermissions
                    }
                    "OPTIONAL" => {
                        installer.optional = true;
                        Argument::None
                    }
                    "MESSAGE_ALWAYS" => {
                        installer.message_always = true;
                        Argument::None
                    }
                    "MESSAGE_LAZY" => {
                        installer.message_lazy = true;
                        Argument::None
                    }
                    _ => {
                        installer.message_never = true;
                        Argument::None
                    }
                }
            }
            _ => return Ok(None),
        };
        Ok(Some(next))
    }

    fn value(&mut self, interp: &Interp, arg: &str, state: Argument) -> Result<Argument, CmdErr> {
        let next = match state {
            Argument::Files => {
                self.files.push(arg.into());
                state
            }
            Argument::Destination => {
                self.destination = if arg.is_empty() || is_full_path(arg) {
                    arg.into()
                } else {
                    format!("{}/{arg}", interp.current_binary_dir())
                };
                Argument::None
            }
            Argument::FilesFromDir => {
                self.files_from_dir = unix_slashes(&source_path(interp, arg));
                Argument::None
            }
            Argument::Pattern | Argument::Regex => {
                let (name, regex) = if matches!(state, Argument::Pattern) {
                    ("PATTERN", format!("/{}$", pattern_regex(arg)))
                } else {
                    ("REGEX", arg.into())
                };
                let regex = Regex::new(&regex)
                    .map_err(|_| CmdErr::Error(format!("could not compile {name} \"{arg}\".")))?;
                self.rules.push(Rule {
                    regex,
                    properties: MatchProperties::default(),
                });
                Argument::None
            }
            Argument::MatchPermissions
            | Argument::FilePermissions
            | Argument::DirectoryPermissions => {
                let bit = mode(arg).ok_or_else(|| {
                    CmdErr::Error(format!(
                        "{} given invalid permission \"{arg}\".",
                        self.name()
                    ))
                })?;
                match state {
                    Argument::MatchPermissions => {
                        self.rules.last_mut().unwrap().properties.mode |= bit
                    }
                    Argument::FilePermissions => *self.file_permissions.as_mut().unwrap() |= bit,
                    _ => *self.directory_permissions.as_mut().unwrap() |= bit,
                }
                state
            }
            Argument::InstallType => {
                self.installer.as_mut().unwrap().kind = match arg {
                    "FILE" => InstallType::File,
                    "PROGRAM" => InstallType::Program,
                    "EXECUTABLE" => InstallType::Executable,
                    "STATIC_LIBRARY" => InstallType::StaticLibrary,
                    "SHARED_LIBRARY" => InstallType::SharedLibrary,
                    "MODULE" => InstallType::Module,
                    "DIRECTORY" => InstallType::Directory,
                    _ => {
                        return Err(CmdErr::Error(format!(
                            "Option TYPE given unknown value \"{arg}\"."
                        )));
                    }
                };
                state
            }
            Argument::Rename => {
                self.installer.as_mut().unwrap().rename = arg.into();
                state
            }
            Argument::None => {
                return Err(CmdErr::Error(format!(
                    "called with unknown argument \"{arg}\"."
                )));
            }
        };
        Ok(next)
    }

    fn prepare_install(&mut self, interp: &Interp) -> CmdResult {
        let installer = self.installer.as_mut().unwrap();
        if !installer.rename.is_empty() {
            if !self.files_from_dir.is_empty() {
                return err("INSTALL option RENAME may not be combined with FILES_FROM_DIR.");
            }
            if !matches!(installer.kind, InstallType::File | InstallType::Program) {
                return err("INSTALL option RENAME may be used only with FILES or PROGRAMS.");
            }
            if self.files.len() > 1 {
                return err("INSTALL option RENAME may be used only with one file.");
            }
        }
        if self.destination.len() < 2 && self.destination != "/" {
            return err("called with inappropriate arguments. No DESTINATION provided or .");
        }
        if let Ok(destdir) = std::env::var("DESTDIR") {
            if !destdir.is_empty() {
                let destdir = unix_slashes(&destdir);
                let destination = &self.destination;
                if destination.starts_with("//") {
                    return err(format!(
                        "called with network path DESTINATION. This does not make sense when using DESTDIR. Specify local absolute path or remove DESTDIR environment variable.\nDESTINATION=\n{destination}"
                    ));
                }
                let suffix = if destination.starts_with('/') {
                    destination.as_str()
                } else if destination.as_bytes().get(1..3) == Some(b":/")
                    && destination.as_bytes()[0].is_ascii_alphabetic()
                {
                    &destination[2..]
                } else {
                    return err(
                        "called with relative DESTINATION. This does not make sense when using DESTDIR. Specify absolute path or remove DESTDIR environment variable.",
                    );
                };
                installer.destdir_len = destdir.len();
                self.destination = format!("{destdir}{suffix}");
            }
        }
        let default_mode = default_directory_mode(interp)?;
        let destination = Path::new(&self.destination);
        if installer.kind != InstallType::Directory {
            if !destination.exists() && mkdir_mode(&self.destination, default_mode).is_err() {
                return err(format!(
                    "cannot create directory: {}. Maybe need administrative privileges.",
                    self.destination
                ));
            }
            if !destination.is_dir() {
                return err(format!(
                    "INSTALL destination: {} is not a directory.",
                    self.destination
                ));
            }
        }
        if u8::from(installer.message_always)
            + u8::from(installer.message_lazy)
            + u8::from(installer.message_never)
            > 1
        {
            return err(
                "INSTALL options MESSAGE_ALWAYS, MESSAGE_LAZY, and MESSAGE_NEVER are mutually exclusive.",
            );
        }
        installer.mode = match std::env::var("CMAKE_INSTALL_MODE").as_deref().unwrap_or("") {
            "" | "COPY" => InstallMode::Copy,
            mode @ ("ABS_SYMLINK"
            | "ABS_SYMLINK_OR_COPY"
            | "REL_SYMLINK"
            | "REL_SYMLINK_OR_COPY"
            | "SYMLINK"
            | "SYMLINK_OR_COPY") => InstallMode::Link {
                try_relative: !mode.starts_with("ABS_"),
                require_relative: mode.starts_with("REL_"),
                or_copy: mode.ends_with("_OR_COPY"),
            },
            value => {
                return err(format!(
                    "Unrecognized value '{value}' for environment variable CMAKE_INSTALL_MODE"
                ));
            }
        };
        Ok(())
    }

    fn run(&mut self, interp: &mut Interp) -> CmdResult {
        let files = std::mem::take(&mut self.files);
        for path in files {
            let mut source = if !path.is_empty() && !is_full_path(&path) {
                if self.files_from_dir.is_empty() {
                    source_path(interp, &path)
                } else {
                    format!("{}/{path}", self.files_from_dir)
                }
            } else if self.files_from_dir.is_empty() {
                path.clone()
            } else {
                return err(
                    "option FILES_FROM_DIR requires all files to be specified as relative paths.",
                );
            };
            source = source.replace('\\', "/");
            if source.starts_with('~') {
                let trailing = source.ends_with('/');
                source = unix_slashes(&source);
                if trailing && !source.ends_with('/') {
                    source.push('/');
                }
            }
            let name = source.rsplit('/').next().unwrap_or_default();
            let mut destination = self.destination.clone();
            if !self.files_from_dir.is_empty() {
                let dir = filename_path(&unix_slashes(&path));
                if !dir.is_empty() {
                    destination = format!("{destination}/{dir}");
                }
            }
            let rename = self
                .installer
                .as_ref()
                .map(|installer| installer.rename.as_str())
                .filter(|name| !name.is_empty());
            let target_name = rename.unwrap_or(name);
            if !target_name.is_empty() {
                destination = format!("{destination}/{target_name}");
            }
            // A trailing separator requests the contents of a directory.
            if name.is_empty() && source.len() > 1 {
                source.pop();
            }
            self.install(interp, &source, &destination)?;
        }
        Ok(())
    }

    fn report(&mut self, interp: &mut Interp, destination: &str, directory: bool, copy: bool) {
        let Some(installer) = &mut self.installer else {
            return;
        };
        if !installer.message_never && (copy || !installer.message_lazy) {
            let action = if copy { "Installing" } else { "Up-to-date" };
            crate::commands::message::status_line(interp, &format!("{action}: {destination}"));
        }
        if !directory {
            if !installer.manifest.is_empty() {
                installer.manifest.push(';');
            }
            installer
                .manifest
                .push_str(&destination[installer.destdir_len..]);
        }
    }

    fn properties(&self, source: &str) -> MatchProperties {
        let mut properties = MatchProperties::default();
        let mut matched = false;
        for rule in &self.rules {
            if rule.regex.find(source).is_some() {
                matched = true;
                properties.exclude |= rule.properties.exclude;
                properties.mode |= rule.properties.mode;
            }
        }
        if !matched && self.matching_only {
            properties.exclude = !Path::new(source).is_dir();
        }
        properties
    }

    fn install(&mut self, interp: &mut Interp, source: &str, destination: &str) -> CmdResult {
        if source.is_empty() {
            if self
                .installer
                .as_ref()
                .is_some_and(|installer| installer.kind == InstallType::Directory)
            {
                return self.directory(interp, source, destination, 0);
            }
            return err("INSTALL encountered an empty string input file name.");
        }
        let properties = self.properties(source);
        if properties.exclude || same_file(source, destination) {
            return Ok(());
        }
        let (source, destination) = if self.follow_chain {
            self.chain(interp, source, destination)?
        } else {
            (source.to_owned(), destination.to_owned())
        };
        let metadata = match fs::symlink_metadata(&source) {
            Ok(metadata) => metadata,
            Err(_)
                if self
                    .installer
                    .as_ref()
                    .is_some_and(|installer| installer.optional) =>
            {
                return Ok(());
            }
            Err(error) => {
                return err(format!(
                    "{} cannot find \"{source}\": {}.",
                    self.name(),
                    system_error(&error)
                ));
            }
        };
        if metadata.is_symlink() {
            let target = fs::read_link(&source).map_err(|error| {
                CmdErr::Error(format!(
                    "{} cannot read symlink \"{source}\" to duplicate at \"{destination}\": {}.",
                    self.name(),
                    system_error(&error)
                ))
            })?;
            let target = target.to_string_lossy();
            if let Err(error) = self.install_link(interp, &target, &destination) {
                let reason = if error.kind() == io::ErrorKind::AlreadyExists
                    && Path::new(&destination).is_dir()
                {
                    "A directory already exists at that location".into()
                } else {
                    system_error(&error)
                };
                return err(format!(
                    "{} cannot duplicate symlink\n  {source}\nat\n  {destination}\nbecause: {reason}",
                    self.name()
                ));
            }
            Ok(())
        } else if metadata.is_dir() {
            self.directory(interp, &source, &destination, properties.mode)
        } else {
            self.file(interp, &source, &destination, properties.mode)
        }
    }

    fn chain(
        &mut self,
        interp: &mut Interp,
        source: &str,
        destination: &str,
    ) -> Result<(String, String), CmdErr> {
        let destination_dir = filename_path(&unix_slashes(destination));
        let mut source = source.to_owned();
        let mut destination = destination.to_owned();
        let mut visited = HashSet::new();
        while let Ok(target) = fs::read_link(&source) {
            if !visited.insert(collapse_from_cwd(&source)) {
                return Err(CmdErr::Error(format!(
                    "{} encountered a symlink cycle at \"{source}\".",
                    self.name()
                )));
            }
            let target = target.to_string_lossy();
            source = if is_full_path(&target) {
                target.into_owned()
            } else {
                format!("{}/{target}", filename_path(&unix_slashes(&source)))
            };
            let name = filename_name(&source);
            self.install_link(interp, &name, &destination)
                .map_err(|error| {
                    CmdErr::Error(format!(
                        "{} cannot create symlink\n  {destination}\nbecause: {}",
                        self.name(),
                        system_error(&error)
                    ))
                })?;
            destination = format!("{destination_dir}/{name}");
        }
        Ok((source, destination))
    }

    fn install_link(
        &mut self,
        interp: &mut Interp,
        target: &str,
        destination: &str,
    ) -> io::Result<()> {
        let copy =
            self.always || !fs::read_link(destination).is_ok_and(|path| path.as_os_str() == target);
        self.report(interp, destination, false, copy);
        if copy {
            let _ = fs::remove_file(destination);
            let _ = mkdir(&filename_path(&unix_slashes(destination)));
            symlink(target, destination)?;
        }
        Ok(())
    }

    fn file(
        &mut self,
        interp: &mut Interp,
        source: &str,
        destination: &str,
        matched_mode: u32,
    ) -> CmdResult {
        let install_mode = self
            .installer
            .as_ref()
            .map(|installer| installer.mode)
            .unwrap_or_default();
        if let InstallMode::Link {
            try_relative,
            require_relative,
            or_copy,
        } = install_mode
        {
            let base = filename_path(destination);
            let candidate = try_relative
                .then(|| relative(source, &base))
                .filter(|path| !path.is_empty());
            let target = candidate.as_deref().unwrap_or(source);
            if candidate.is_some() || !require_relative {
                if let Err(error) = self.install_link(interp, target, destination) {
                    if !or_copy {
                        return err(format!(
                            "{} cannot create symlink to \"{target}\" at \"{destination}\": {}\".",
                            self.name(),
                            system_error(&error)
                        ));
                    }
                } else {
                    return Ok(());
                }
            } else if !or_copy {
                return err(format!(
                    "{} cannot determine relative path for symlink to \"{target}\" at \"{destination}\".",
                    self.name()
                ));
            }
        }
        let time = |path| {
            fs::metadata(path)
                .and_then(|metadata| metadata.modified())
                .ok()
        };
        let copy = self.always
            || time(source)
                .zip(time(destination))
                .is_none_or(|(left, right)| {
                    let difference = left
                        .duration_since(right)
                        .unwrap_or_else(|error| error.duration());
                    difference >= Duration::from_secs(1)
                });
        self.report(interp, destination, false, copy);
        if copy {
            let copy_result = copy_file_always(source, destination);
            if let Err(error) = copy_result {
                return err(format!(
                    "{} cannot copy file \"{source}\" to \"{destination}\": {}.",
                    self.name(),
                    system_error(&error)
                ));
            }
            if !self.always {
                if let Ok(meta) = fs::metadata(destination) {
                    let _ = fs::set_permissions(
                        destination,
                        fs::Permissions::from_mode(meta.permissions().mode() | 0o200),
                    );
                }
                if let Err(error) = copy_times(source, destination) {
                    return err(format!(
                        "{} cannot set modification time on \"{destination}\": {}.",
                        self.name(),
                        system_error(&error)
                    ));
                }
            }
        }
        let requested = if matched_mode != 0 {
            matched_mode
        } else {
            self.file_permissions.unwrap_or(0)
        };
        self.set_permissions(destination, source_mode(source, requested))
    }

    fn directory(
        &mut self,
        interp: &mut Interp,
        source: &str,
        destination: &str,
        matched_mode: u32,
    ) -> CmdResult {
        let copy = !fs::symlink_metadata(destination).is_ok_and(|meta| meta.is_dir());
        self.report(interp, destination, true, copy);
        let default_mode = default_directory_mode(interp)?;
        if let Err(error) = mkdir_mode(destination, default_mode) {
            return err(format!(
                "{} cannot make directory \"{destination}\": {}.",
                self.name(),
                system_error(&error)
            ));
        }
        let requested = if matched_mode != 0 {
            matched_mode
        } else {
            self.directory_permissions.unwrap_or(0)
        };
        let permissions = source_mode(source, requested);
        self.set_permissions(destination, permissions | 0o700)?;
        let result = if source.is_empty() {
            Ok(())
        } else {
            fs::read_dir(source)
                .into_iter()
                .flatten()
                .flatten()
                .try_for_each(|entry| {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    self.install(
                        interp,
                        &format!("{source}/{name}"),
                        &format!("{destination}/{name}"),
                    )
                })
        };
        if permissions & 0o700 != 0o700 {
            let restore = self.set_permissions(destination, permissions);
            result.and(restore)
        } else {
            result
        }
    }

    fn set_permissions(&self, destination: &str, bits: u32) -> CmdResult {
        if bits != 0 {
            if let Err(error) = fs::set_permissions(destination, fs::Permissions::from_mode(bits)) {
                return err(format!(
                    "{} cannot set permissions on \"{destination}\": {}.",
                    self.name(),
                    system_error(&error)
                ));
            }
        }
        Ok(())
    }
}

fn before_match(arg: &str) -> CmdErr {
    CmdErr::Error(format!(
        "option {arg} may not appear before PATTERN or REGEX."
    ))
}

fn after_match(arg: &str) -> CmdErr {
    CmdErr::Error(format!(
        "option {arg} may not appear after PATTERN or REGEX."
    ))
}

fn source_mode(source: &str, requested: u32) -> u32 {
    if requested != 0 {
        requested
    } else {
        fs::metadata(source).map_or(0, |meta| meta.permissions().mode())
    }
}

fn default_directory_mode(interp: &Interp) -> Result<Option<u32>, CmdErr> {
    let permissions = interp
        .get_def("CMAKE_INSTALL_DEFAULT_DIRECTORY_PERMISSIONS")
        .unwrap_or_default();
    if permissions.is_empty() {
        return Ok(None);
    }
    let bits = expand_list(permissions, false)
        .into_iter()
        .try_fold(0, |bits, spec| mode(&spec).map(|mode| mode | bits))
        .ok_or_else(|| {
            CmdErr::Error(" Set with CMAKE_INSTALL_DEFAULT_DIRECTORY_PERMISSIONS variable.".into())
        })?;
    Ok(Some(bits))
}

fn mkdir_mode(path: &str, mode: Option<u32>) -> io::Result<()> {
    let path = unix_slashes(path);
    if let Ok(meta) = fs::metadata(&path) {
        return if meta.is_dir() {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(libc::EEXIST))
        };
    }
    // KWSys creates each literal prefix, including directories before `..`.
    let create = |dir: &str| match fs::create_dir(dir) {
        Ok(()) => match mode {
            Some(mode) => fs::set_permissions(dir, fs::Permissions::from_mode(mode)),
            None => Ok(()),
        },
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists && Path::new(dir).is_dir() => {
            Ok(())
        }
        Err(error) => Err(error),
    };
    for (slash, _) in path.match_indices('/') {
        if slash != 0 {
            let _ = create(&path[..slash]);
        }
    }
    create(&path)
}
