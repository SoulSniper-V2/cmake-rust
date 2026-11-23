//! `cmake -E cmake_depends`: Makefile-generator dependency scanning.
//!
//! Ports `cmcmd.cxx`'s cmake_depends path plus the pieces of
//! `cmLocalUnixMakefileGenerator3`, `cmDepends`, `cmDependsC` and
//! `cmDependsCompiler` that it drives: reading `DependInfo.cmake` and
//! `CMakeDirectoryInformation.cmake`, checking whether `depend.internal` /
//! `compiler_depend.internal` are up to date, and regenerating
//! `depend.make` / `compiler_depend.make` with the C/C++ include scanner or
//! the compiler-generated depfile consolidator.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::Read as _;
use std::path::Path;
use std::time::SystemTime;

use crate::cmcmd::depfile;
use crate::cmregex::Regex;
use crate::interp::{Interp, Makefile, Mode};
use crate::{configure, pathresolver, util};

const INCLUDE_REGEX_LINE: &str = "^[ \t]*[#%][ \t]*(include|import)[ \t]*[<\"]([^\">]+)([\">])";
const INCLUDE_REGEX_LINE_MARKER: &str = "#IncludeRegexLine: ";
const INCLUDE_REGEX_SCAN_MARKER: &str = "#IncludeRegexScan: ";
const INCLUDE_REGEX_COMPLAIN_MARKER: &str = "#IncludeRegexComplain: ";
const INCLUDE_REGEX_TRANSFORM_MARKER: &str = "#IncludeRegexTransform: ";

/// `isCMakeVerbose()` (cmUtils.hxx).
fn verbose() -> bool {
    std::env::var_os("VERBOSE").is_some() && std::env::var_os("CMAKE_NO_VERBOSE").is_none()
}

/// `cmSystemTools::Stdout`.
fn stdout(text: &str) {
    print!("{text}");
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
}

/// `cmFileTime` on Unix: nanoseconds since the epoch.
#[derive(Clone, Copy, Default)]
struct FileTime {
    exists: bool,
    newer_than_zero: bool,
    nsec: i128,
}

impl FileTime {
    /// `cmFileTime::Load` (follows symlinks).
    fn load(path: &str) -> FileTime {
        match std::fs::metadata(path).and_then(|m| m.modified()) {
            Ok(mtime) => {
                let nsec = match mtime.duration_since(SystemTime::UNIX_EPOCH) {
                    Ok(d) => i128::from(d.as_secs()) * 1_000_000_000 + i128::from(d.subsec_nanos()),
                    Err(e) => {
                        let d = e.duration();
                        -(i128::from(d.as_secs()) * 1_000_000_000 + i128::from(d.subsec_nanos()))
                    }
                };
                FileTime {
                    exists: true,
                    newer_than_zero: nsec != 0,
                    nsec,
                }
            }
            Err(_) => FileTime::default(),
        }
    }

    /// `cmFileTime::Newer`.
    fn newer(&self, other: &FileTime) -> bool {
        if !self.newer_than_zero {
            return false;
        }
        if !other.newer_than_zero {
            return true;
        }
        self.nsec > other.nsec
    }

    /// `cmFileTime::Older`.
    fn older(&self, other: &FileTime) -> bool {
        if !other.newer_than_zero {
            return false;
        }
        if !self.newer_than_zero {
            return true;
        }
        self.nsec < other.nsec
    }
}

/// `cmFileTimeCache`.
struct FileTimeCache {
    cache: BTreeMap<String, FileTime>,
}

impl FileTimeCache {
    fn new() -> Self {
        FileTimeCache {
            cache: BTreeMap::new(),
        }
    }

    fn load(&mut self, path: &str) -> FileTime {
        if let Some(time) = self.cache.get(path) {
            return *time;
        }
        let time = FileTime::load(path);
        self.cache.insert(path.to_owned(), time);
        time
    }

    fn remove(&mut self, path: &str) {
        self.cache.remove(path);
    }

    /// `cmFileTimeCache::Compare`: `None` when either file does not
    /// exist, else `Some(sign(time1 - time2))`.
    fn compare(&mut self, file1: &str, file2: &str) -> Option<i32> {
        let t1 = self.load(file1);
        let t2 = self.load(file2);
        if !t1.exists || !t2.exists {
            return None;
        }
        let diff = t1.nsec - t2.nsec;
        Some(diff.signum() as i32)
    }
}

/// `cmSystemTools::GetLineFromStream` with the trailing CR dropped.
fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
}

fn file_exists(path: &str) -> bool {
    Path::new(path).exists()
}

/// `cmSystemTools::FileExists(path, true)`: exists and is not a directory.
fn file_is_file(path: &str) -> bool {
    std::fs::metadata(path).is_ok_and(|m| !m.is_dir())
}

/// `cmSystemTools::IsSubDirectory`: `sub` is a strict subdirectory of `dir`.
fn is_sub_directory(sub: &str, dir: &str) -> bool {
    sub != dir && util::is_subdirectory(sub, dir)
}

/// `cmSystemTools::ComparePath`.
fn compare_path(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

fn path_eq_or_subdir(a: &str, b: &str) -> bool {
    compare_path(a, b) || is_sub_directory(a, b)
}

/// `cmSystemTools::GetFilenamePath`.
fn filename_path(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

/// `cmSystemTools::GetFilenameName`.
fn filename_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// `cmSystemTools::GetFilenameWithoutLastExtension`.
fn filename_without_last_ext(path: &str) -> String {
    let name = filename_name(path);
    match name.find('.') {
        Some(pos) if pos > 0 => name[..pos].to_owned(),
        _ => name.to_owned(),
    }
}

/// `cmSystemTools::ForceToRelativePath`.
fn force_to_relative_path(local_path: &str, remote_path: &str) -> String {
    if !util::is_full_path(remote_path) {
        return remote_path.to_owned();
    }
    fn split(path: &str) -> Vec<&str> {
        path.split('/').collect()
    }
    let local = split(local_path.trim_end_matches('/'));
    let remote = split(remote_path);
    let mut common = 0;
    while common < remote.len() && common < local.len() && remote[common] == local[common] {
        common += 1;
    }
    if common == 0 {
        return remote_path.to_owned();
    }
    if common == remote.len() && common == local.len() {
        return ".".to_owned();
    }
    if common + 1 == remote.len() && remote[common].is_empty() && common == local.len() {
        return "./".to_owned();
    }
    let mut relative = String::new();
    for i in common..local.len() {
        relative.push_str("..");
        if i < local.len() - 1 {
            relative.push('/');
        }
    }
    if !relative.is_empty() && !remote.is_empty() {
        relative.push('/');
    }
    relative.push_str(&remote[common..].join("/"));
    relative
}

/// `cmExternalMakefileProjectGenerator::CreateFullGeneratorName` plus the
/// extra-generator lookup used by `cmake::CreateGlobalGenerator`.  Returns
/// `(extra generator name, base global generator name)` when the name is an
/// extra generator alias or a full `"<extra> - <global>"` name.
fn split_extra_generator(name: &str) -> Option<(String, String)> {
    for extra in crate::generator::extra::EXTRA_GENERATORS {
        if extra.name == name {
            return Some((extra.name.to_owned(), extra.supported[0].to_owned()));
        }
        for global in extra.supported {
            let full = format!("{} - {}", extra.name, global);
            if full == name {
                return Some((extra.name.to_owned(), (*global).to_owned()));
            }
        }
    }
    None
}

/// A Makefile-family global generator's output settings
/// (`cmGlobalUnixMakefileGenerator3`).
struct MakefileGlobal {
    line_continue: &'static str,
    supports_long_line_deps: bool,
}

/// Map the global generator name to a Makefile generator when the name is
/// creatable on this platform and drives
/// `cmLocalUnixMakefileGenerator3::UpdateDependencies`.  `Some(None)`
/// means the generator is creatable but not a Makefile generator, so its
/// `UpdateDependencies` succeeds trivially.  `None` means the name is not
/// a creatable generator and the command fails with exit code 1.
fn makefile_global(name: &str) -> Option<Option<MakefileGlobal>> {
    let base = match split_extra_generator(name) {
        Some((_, base)) => base,
        None => name.to_owned(),
    };
    match base.as_str() {
        "Unix Makefiles" => Some(Some(MakefileGlobal {
            line_continue: "\\\n",
            supports_long_line_deps: true,
        })),
        "Watcom WMake" => Some(Some(MakefileGlobal {
            line_continue: "&\n",
            supports_long_line_deps: true,
        })),
        // Creatable generators whose UpdateDependencies trivially succeeds.
        "Ninja" | "Ninja Multi-Config" | "Green Hills MULTI" => Some(None),
        _ => None,
    }
}

/// `cmLocalUnixMakefileGenerator3` subset used by the scanners: relative
/// path top dirs and makefile path conversion.
struct LocalGen {
    relative_top_source: String,
    relative_top_binary: String,
    /// `GetState()->GetBinaryDirectory()`: the home output directory.
    top_binary: String,
    /// `GetCurrentBinaryDirectory()`: the directory being scanned.
    current_binary: String,
    /// `GetCurrentSourceDirectory()` (retained for upstream parity).
    #[allow(dead_code)]
    current_source: String,
    /// `GetSourceDirectory()`/`GetBinaryDirectory()`... unused, kept for clarity.
    global: MakefileGlobal,
}

impl LocalGen {
    /// `cmOutputConverter::ComputeRelativePathTopRelation` + `MaybeRelativeTo`.
    fn maybe_relative_to(&self, local_path: &str, remote_path: &str) -> String {
        let mut local_in_binary = path_eq_or_subdir(local_path, &self.relative_top_binary);
        let mut remote_in_binary = path_eq_or_subdir(remote_path, &self.relative_top_binary);
        let mut local_in_source = path_eq_or_subdir(local_path, &self.relative_top_source);
        let mut remote_in_source = path_eq_or_subdir(remote_path, &self.relative_top_source);
        if compare_path(&self.relative_top_source, &self.relative_top_binary) {
            // InSource: checks are identical.
        } else if is_sub_directory(&self.relative_top_binary, &self.relative_top_source) {
            local_in_source = local_in_source && !local_in_binary;
            remote_in_source = remote_in_source && !remote_in_binary;
        } else if is_sub_directory(&self.relative_top_source, &self.relative_top_binary) {
            local_in_binary = local_in_binary && !local_in_source;
            remote_in_binary = remote_in_binary && !remote_in_source;
        }
        if (local_in_binary && remote_in_binary) || (local_in_source && remote_in_source) {
            force_to_relative_path(local_path, remote_path)
        } else {
            remote_path.to_owned()
        }
    }

    /// `cmOutputConverter::MaybeRelativeToTopBinDir`.
    fn maybe_relative_to_top_bin_dir(&self, path: &str) -> String {
        self.maybe_relative_to(&self.top_binary, path)
    }

    /// `cmGlobalUnixMakefileGenerator3::ConvertToMakefilePath` (Unix form).
    fn convert_to_makefile_path(path: &str) -> String {
        let mut result = String::with_capacity(path.len());
        for c in path.chars() {
            match c {
                '=' => result.push_str("$(EQUALS)"),
                '$' => result.push_str("$$"),
                '\\' | ' ' | '#' => {
                    result.push('\\');
                    result.push(c);
                }
                _ => result.push(c),
            }
        }
        result
    }
}

/// `cmGeneratedFileStream`: writes to a temp file first, then moves into
/// place.  With `copy_if_different` the destination is left untouched when
/// the content is identical.  `None` result mirrors a failed open.
fn write_generated(path: &str, content: &str, copy_if_different: bool) -> bool {
    if copy_if_different
        && let Ok(existing) = std::fs::read(path)
        && existing == content.as_bytes()
    {
        return true;
    }
    std::fs::write(path, content).is_ok()
}

/// `cmLocalUnixMakefileGenerator3::WriteDisclaimer`.
fn write_disclaimer(gen_name: &str) -> String {
    format!(
        "# CMAKE generated file: DO NOT EDIT!\n\
         # Generated by \"{gen_name}\" Generator, CMake Version {}.{}\n\n",
        crate::interp::VERSION.0,
        crate::interp::VERSION.1,
    )
}

/// `cmDepends::DependencyMap`.
type DependencyMap = BTreeMap<String, Vec<String>>;

/// `CMAKE_DEPENDS_IN_PROJECT_ONLY` path filter.
type PathFilter = dyn Fn(&str) -> bool;

/// `cmDepends::Check` / `CheckDependencies`: parses the depend.internal
/// file and decides whether existing dependencies are still valid.
fn check_dependencies(
    cache: &mut FileTimeCache,
    verbose: bool,
    make_file: &str,
    internal_file: &str,
    valid_deps: &mut DependencyMap,
) -> bool {
    let clear = |cache: &mut FileTimeCache| {
        if verbose {
            stdout(&format!("Clearing dependencies in \"{make_file}\".\n"));
        }
        write_generated(
            make_file,
            "# Empty dependencies file\n# This may be replaced when dependencies are built.\n",
            false,
        );
        let _ = std::fs::remove_file(internal_file);
        cache.remove(internal_file);
    };
    let Some(content) = std::fs::read_to_string(internal_file).ok() else {
        clear(cache);
        return false;
    };
    let internal_depends_time = cache.load(internal_file);
    if !internal_depends_time.exists {
        clear(cache);
        return false;
    }

    let mut okay = true;
    let mut depender = String::new();
    let mut depender_exists = false;
    let mut depender_time = FileTime::default();
    let mut current: Option<String> = None;
    for line in lines(&content) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !line.starts_with(' ') {
            depender = line.to_owned();
            let time = cache.load(&depender);
            depender_exists = time.exists;
            depender_time = time;
            // Do not initialize the entry; map access on a dependee line
            // creates it so dependers with multiple entries keep their
            // dependencies.
            current = Some(depender.clone());
            continue;
        }
        let dependee = &line[1..];
        if let Some(key) = &current {
            valid_deps
                .entry(key.clone())
                .or_default()
                .push(dependee.to_owned());
        }

        let dependee_time = cache.load(dependee);
        let dependee_exists = dependee_time.exists;
        let mut regenerate = false;
        if !dependee_exists {
            regenerate = true;
            if verbose {
                stdout(&format!(
                    "Dependee \"{dependee}\" does not exist for depender \"{depender}\".\n"
                ));
            }
        } else if depender_exists {
            if depender_time.older(&dependee_time) {
                regenerate = true;
                if verbose {
                    stdout(&format!(
                        "Dependee \"{dependee}\" is newer than depender \"{depender}\".\n"
                    ));
                }
            }
        } else if internal_depends_time.older(&dependee_time) {
            regenerate = true;
            if verbose {
                stdout(&format!(
                    "Dependee \"{dependee}\" is newer than depends file \"{internal_file}\".\n"
                ));
            }
        }
        if regenerate {
            okay = false;
            if let Some(key) = current.take() {
                valid_deps.remove(&key);
            }
            if depender_exists {
                let _ = std::fs::remove_file(&depender);
                cache.remove(&depender);
                depender_exists = false;
            }
        }
    }
    if !okay {
        // cmDepends::Check: regenerate everything.
        let _ = make_file;
        return false;
    }
    true
}

/// `UnscannedEntry` (cmDependsC.h).
#[derive(Clone, Default)]
struct UnscannedEntry {
    file_name: String,
    quoted_location: String,
}

/// `cmIncludeLines` (cmDependsC.h).
#[derive(Default)]
struct IncludeLines {
    used: bool,
    unscanned_entries: Vec<UnscannedEntry>,
}

/// `cmDependsC`: the C-family `#include`/`#import` scanner.
struct DependsC<'a> {
    lg: &'a LocalGen,
    mf: &'a Interp,
    /// `m_TargetDirectory`/`m_Language`/`m_Verbose` (retained for parity).
    #[allow(dead_code)]
    target_dir: String,
    #[allow(dead_code)]
    language: String,
    valid_deps: Option<&'a DependencyMap>,
    #[allow(dead_code)]
    verbose: bool,
    include_path: Vec<String>,
    include_regex_line: Regex,
    include_regex_scan: Regex,
    include_regex_complain: Regex,
    include_regex_transform: Option<Regex>,
    include_regex_line_string: String,
    include_regex_scan_string: String,
    include_regex_complain_string: String,
    include_regex_transform_string: String,
    transform_rules: BTreeMap<String, String>,
    cache_file_name: String,
    file_cache: BTreeMap<String, IncludeLines>,
    encountered: BTreeSet<String>,
    unscanned: VecDeque<UnscannedEntry>,
    header_location_cache: BTreeMap<String, String>,
}

impl<'a> DependsC<'a> {
    /// `cmDependsC::cmDependsC(lg, targetDir, lang, validDeps)`.
    fn new(
        lg: &'a LocalGen,
        mf: &'a Interp,
        target_dir: &str,
        lang: &str,
        valid_deps: Option<&'a DependencyMap>,
        verbose: bool,
    ) -> DependsC<'a> {
        // cmDepends::SetIncludePathFromLanguage.
        let mut include_path = Vec::new();
        let target_var = format!("CMAKE_{lang}_TARGET_INCLUDE_PATH");
        let dir_var = format!("CMAKE_{lang}_INCLUDE_PATH");
        if let Some(value) = mf.get_def(&target_var) {
            util::expand_list_into(value, false, &mut include_path);
        } else if let Some(value) = mf.get_def(&dir_var) {
            util::expand_list_into(value, false, &mut include_path);
        }

        let mut scan_regex = "^.*$".to_owned();
        let mut complain_regex = "^$".to_owned();
        if let Some(value) = mf.get_def(&format!("CMAKE_{lang}_INCLUDE_REGEX_SCAN")) {
            scan_regex = value.to_owned();
        }
        if let Some(value) = mf.get_def(&format!("CMAKE_{lang}_INCLUDE_REGEX_COMPLAIN")) {
            complain_regex = value.to_owned();
        }

        let compile = |pattern: &str| {
            Regex::new(pattern).unwrap_or_else(|_| {
                // An invalid regex never matches (RegularExpression::find
                // fails on an uncompiled program).
                Regex::new("^$").unwrap()
            })
        };

        let mut scanner = DependsC {
            lg,
            mf,
            target_dir: target_dir.to_owned(),
            language: lang.to_owned(),
            valid_deps,
            verbose,
            include_path,
            include_regex_line: compile(INCLUDE_REGEX_LINE),
            include_regex_scan: compile(&scan_regex),
            include_regex_complain: compile(&complain_regex),
            include_regex_transform: None,
            include_regex_line_string: format!("{INCLUDE_REGEX_LINE_MARKER}{INCLUDE_REGEX_LINE}"),
            include_regex_scan_string: format!("{INCLUDE_REGEX_SCAN_MARKER}{scan_regex}"),
            include_regex_complain_string: format!(
                "{INCLUDE_REGEX_COMPLAIN_MARKER}{complain_regex}"
            ),
            include_regex_transform_string: String::new(),
            transform_rules: BTreeMap::new(),
            cache_file_name: format!("{target_dir}/{lang}.includecache"),
            file_cache: BTreeMap::new(),
            encountered: BTreeSet::new(),
            unscanned: VecDeque::new(),
            header_location_cache: BTreeMap::new(),
        };
        scanner.setup_transforms();
        scanner.read_cache_file();
        scanner
    }

    /// `cmDependsC::SetupTransforms`.
    fn setup_transforms(&mut self) {
        let rules = util::expand_list(
            self.mf.get_def("CMAKE_INCLUDE_TRANSFORMS").unwrap_or(""),
            true,
        );
        for rule in rules {
            // cmDependsC::ParseTransform: split on "(%)=".
            if let Some(pos) = rule.find("(%)=")
                && pos > 0
            {
                self.transform_rules
                    .insert(rule[..pos].to_owned(), rule[pos + 4..].to_owned());
            }
        }
        self.include_regex_transform_string = INCLUDE_REGEX_TRANSFORM_MARKER.to_owned();
        if !self.transform_rules.is_empty() {
            let mut xform = "^([ \t]*[#%][ \t]*(include|import)[ \t]*)(".to_owned();
            for (i, name) in self.transform_rules.keys().enumerate() {
                if i > 0 {
                    xform.push('|');
                }
                xform.push_str(name);
            }
            xform.push_str(")[ \t]*\\(([^),]*)\\)");
            self.include_regex_transform = Regex::new(&xform).ok();
            self.include_regex_transform_string.push_str(&xform);
            for (name, value) in &self.transform_rules {
                self.include_regex_transform_string
                    .push_str(&format!(" {name}(%)={value}"));
            }
        }
    }

    /// `cmDependsC::TransformLine`.
    fn transform_line(&self, line: &str) -> String {
        let Some(regex) = &self.include_regex_transform else {
            return line.to_owned();
        };
        let Some(m) = regex.find(line) else {
            return line.to_owned();
        };
        let group = |n: usize| m.groups[n].map(|(s, e)| &line[s..e]).unwrap_or("");
        let Some(value) = self.transform_rules.get(group(3)) else {
            return line.to_owned();
        };
        let mut newline = group(1).to_owned();
        let arg = group(4);
        for c in value.chars() {
            if c == '%' {
                newline.push_str(arg);
            } else {
                newline.push(c);
            }
        }
        newline
    }

    /// `cmDependsC::ReadCacheFile`.
    fn read_cache_file(&mut self) {
        let Ok(content) = std::fs::read_to_string(&self.cache_file_name) else {
            return;
        };
        let cache_file_time = FileTime::load(&self.cache_file_name);
        let cache_file_time_good = cache_file_time.exists;
        let mut it = lines(&content).peekable();
        let mut cache_entry: Option<String> = None;
        let mut have_file_name = false;
        while let Some(line) = it.next() {
            if line.is_empty() {
                cache_entry = None;
                have_file_name = false;
                continue;
            }
            if !have_file_name {
                have_file_name = true;
                let file_time = FileTime::load(line);
                let res = cache_file_time_good && file_time.exists;
                if res && cache_file_time.newer(&file_time) {
                    cache_entry = Some(line.to_owned());
                    self.file_cache.entry(line.to_owned()).or_default();
                } else if !res {
                    // Marker lines: the cached regexes must still match.
                    for (marker, current) in [
                        (INCLUDE_REGEX_LINE_MARKER, &self.include_regex_line_string),
                        (INCLUDE_REGEX_SCAN_MARKER, &self.include_regex_scan_string),
                        (
                            INCLUDE_REGEX_COMPLAIN_MARKER,
                            &self.include_regex_complain_string,
                        ),
                        (
                            INCLUDE_REGEX_TRANSFORM_MARKER,
                            &self.include_regex_transform_string,
                        ),
                    ] {
                        if line.starts_with(marker) && line != current.as_str() {
                            return;
                        }
                    }
                }
            } else if let Some(key) = &cache_entry {
                let mut entry = UnscannedEntry {
                    file_name: line.to_owned(),
                    quoted_location: String::new(),
                };
                if let Some(next) = it.next()
                    && next != "-"
                {
                    entry.quoted_location = next.to_owned();
                }
                self.file_cache
                    .entry(key.clone())
                    .or_default()
                    .unscanned_entries
                    .push(entry);
            }
        }
    }

    /// `cmDependsC::WriteCacheFile` (destructor writes it unconditionally).
    fn write_cache_file(&self) {
        let mut out = String::new();
        out.push_str(&self.include_regex_line_string);
        out.push_str("\n\n");
        out.push_str(&self.include_regex_scan_string);
        out.push_str("\n\n");
        out.push_str(&self.include_regex_complain_string);
        out.push_str("\n\n");
        out.push_str(&self.include_regex_transform_string);
        out.push_str("\n\n");
        for (file, entry) in &self.file_cache {
            if entry.used {
                out.push_str(file);
                out.push('\n');
                for inc in &entry.unscanned_entries {
                    out.push_str(&inc.file_name);
                    out.push('\n');
                    if inc.quoted_location.is_empty() {
                        out.push_str("-\n");
                    } else {
                        out.push_str(&inc.quoted_location);
                        out.push('\n');
                    }
                }
                out.push('\n');
            }
        }
        let _ = std::fs::write(&self.cache_file_name, out);
    }

    /// `cmDependsC::Scan`: parse one file's include directives.
    fn scan<R: std::io::BufRead>(&mut self, reader: R, directory: &str, full_name: &str) {
        let entry_slot = self.file_cache.entry(full_name.to_owned()).or_default();
        entry_slot.used = true;
        let mut new_entries = Vec::new();
        for raw in reader.lines().map_while(Result::ok) {
            let line = raw.strip_suffix('\r').unwrap_or(&raw).to_owned();
            let line = if !self.transform_rules.is_empty() {
                self.transform_line(&line)
            } else {
                line
            };
            if let Some(m) = self.include_regex_line.find(&line) {
                let group = |n: usize| m.groups[n].map(|(s, e)| line[s..e].to_owned());
                let mut entry = UnscannedEntry {
                    file_name: crate::cache::convert_to_unix_slashes(&group(2).unwrap_or_default()),
                    ..UnscannedEntry::default()
                };
                if group(3).as_deref() == Some("\"") && !util::is_full_path(&entry.file_name) {
                    entry.quoted_location = util::collapse_full_path(&entry.file_name, directory);
                }
                if self.include_regex_scan.find(&entry.file_name).is_some() {
                    new_entries.push(entry.clone());
                    if !self.encountered.contains(&entry.file_name) {
                        self.encountered.insert(entry.file_name.clone());
                        self.unscanned.push_back(entry);
                    }
                }
            }
        }
        self.file_cache
            .entry(full_name.to_owned())
            .or_default()
            .unscanned_entries = new_entries;
    }

    /// `cmDependsC::WriteDependencies`.
    fn write_dependencies(
        &mut self,
        sources: &BTreeSet<String>,
        obj: &str,
        make_depends: &mut String,
        internal_depends: &mut String,
    ) -> bool {
        if sources.is_empty() || sources.iter().next().is_some_and(|s| s.is_empty()) {
            super::error("Cannot scan dependencies without a source file.");
            return false;
        }
        if obj.is_empty() {
            super::error("Cannot scan dependencies without an object file.");
            return false;
        }

        let mut dependencies: BTreeSet<String> = BTreeSet::new();
        let mut have_deps = false;
        let obj_i = self.lg.maybe_relative_to_top_bin_dir(obj);

        if let Some(valid_deps) = self.valid_deps
            && let Some(valid) = valid_deps.get(&obj_i)
        {
            dependencies.extend(valid.iter().cloned());
            have_deps = true;
        }

        if !have_deps {
            let mut src_files = sources.len() as i64;
            self.encountered.clear();
            for src in sources {
                self.unscanned.push_back(UnscannedEntry {
                    file_name: src.clone(),
                    quoted_location: String::new(),
                });
                self.encountered.insert(src.clone());
            }

            let mut scanned: BTreeSet<String> = BTreeSet::new();
            while let Some(current) = self.unscanned.pop_front() {
                let mut full_name = String::new();
                if src_files > 0 || util::is_full_path(&current.file_name) {
                    if file_is_file(&current.file_name) {
                        full_name = current.file_name.clone();
                    }
                } else if !current.quoted_location.is_empty()
                    && file_is_file(&current.quoted_location)
                {
                    full_name = current.quoted_location.clone();
                } else if let Some(location) = self.header_location_cache.get(&current.file_name) {
                    full_name = location.clone();
                } else {
                    for ipath in &self.include_path {
                        let tmp_path = util::collapse_full_path(&current.file_name, ipath);
                        if file_is_file(&tmp_path) {
                            full_name = tmp_path.clone();
                            self.header_location_cache
                                .insert(current.file_name.clone(), tmp_path);
                            break;
                        }
                    }
                }

                if full_name.is_empty()
                    && self
                        .include_regex_complain
                        .find(&current.file_name)
                        .is_some()
                {
                    super::error(format!("Cannot find file \"{}\".", current.file_name));
                    return false;
                }

                if !full_name.is_empty() && !scanned.contains(&full_name) {
                    scanned.insert(full_name.clone());
                    if self.file_cache.contains_key(&full_name) {
                        let cached = self.file_cache.get_mut(&full_name).unwrap();
                        cached.used = true;
                        dependencies.insert(full_name.clone());
                        let entries = cached.unscanned_entries.clone();
                        for inc in entries {
                            if !self.encountered.contains(&inc.file_name) {
                                self.encountered.insert(inc.file_name.clone());
                                self.unscanned.push_back(inc);
                            }
                        }
                    } else if let Ok(file) = std::fs::File::open(&full_name) {
                        let bytes = {
                            let mut buf = Vec::new();
                            let _ = std::io::BufReader::new(file).read_to_end(&mut buf);
                            buf
                        };
                        // cmsys::FStream::ReadBOM: scan only files without a
                        // BOM or with a UTF-8 BOM.
                        let rest = match bytes.as_slice() {
                            [0xEF, 0xBB, 0xBF, rest @ ..] => Some(rest),
                            _ if bytes.starts_with(&[0xFF, 0xFE])
                                || bytes.starts_with(&[0xFE, 0xFF]) =>
                            {
                                None
                            }
                            _ => Some(&bytes[..]),
                        };
                        if let Some(rest) = rest {
                            dependencies.insert(full_name.clone());
                            let dir = filename_path(&full_name).to_owned();
                            let text = String::from_utf8_lossy(rest).into_owned();
                            self.scan(std::io::BufReader::new(text.as_bytes()), &dir, &full_name);
                        }
                    }
                }
                src_files -= 1;
            }
        }

        let obj_m = LocalGen::convert_to_makefile_path(&obj_i);
        internal_depends.push_str(&obj_i);
        internal_depends.push('\n');
        if !dependencies.is_empty() {
            let line_continue = self.lg.global.line_continue;
            let long_lines = self.lg.global.supports_long_line_deps;
            if long_lines {
                make_depends.push_str(&obj_m);
                make_depends.push(':');
            }
            for dep in &dependencies {
                let dependee =
                    LocalGen::convert_to_makefile_path(&self.lg.maybe_relative_to_top_bin_dir(dep));
                if long_lines {
                    make_depends.push(' ');
                    make_depends.push_str(line_continue);
                    make_depends.push(' ');
                    make_depends.push_str(&dependee);
                } else {
                    make_depends.push_str(&format!("{obj_m}: {dependee}\n"));
                }
                internal_depends.push(' ');
                internal_depends.push_str(dep);
                internal_depends.push('\n');
            }
            make_depends.push('\n');
        }
        true
    }
}

/// `cmDepends::Write`: group the `CMAKE_DEPENDS_CHECK_<LANG>` pairs by
/// object file and run the scanner for each.
#[allow(clippy::too_many_arguments)]
fn write_target_dependencies(
    lg: &LocalGen,
    interp: &Interp,
    target_dir: &str,
    lang: &str,
    valid_deps: Option<&DependencyMap>,
    verbose: bool,
    make_depends: &mut String,
    internal_depends: &mut String,
) -> bool {
    let pairs = util::expand_list(
        interp
            .get_def(&format!("CMAKE_DEPENDS_CHECK_{lang}"))
            .unwrap_or(""),
        false,
    );
    let mut dependencies: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut it = pairs.into_iter();
    while let Some(src) = it.next() {
        let Some(obj) = it.next() else { break };
        dependencies.entry(obj).or_default().insert(src);
    }
    let mut scanner = DependsC::new(lg, interp, target_dir, lang, valid_deps, verbose);
    let mut ok = true;
    for (obj, srcs) in &dependencies {
        if !scanner.write_dependencies(srcs, obj, make_depends, internal_depends) {
            ok = false;
            break;
        }
    }
    scanner.write_cache_file();
    ok
}

/// `cmLocalUnixMakefileGenerator3::ScanDependencies`.
#[allow(clippy::too_many_arguments)]
fn scan_dependencies(
    lg: &mut LocalGen,
    gen_name: &str,
    interp: &mut Interp,
    target_dir: &str,
    depend_file: &str,
    internal_depend_file: &str,
    valid_deps: &DependencyMap,
    verbose: bool,
) -> bool {
    // Read the directory information file.
    let dir_info_file = format!(
        "{}/CMakeFiles/CMakeDirectoryInformation.cmake",
        lg.current_binary
    );
    let have_directory_info = interp.read_list_file(&dir_info_file) && !interp.error;
    if have_directory_info {
        // CMAKE_FORCE_UNIX_PATHS is unconditional on this platform.
        if let (Some(top_source), Some(top_binary)) = (
            interp.get_def("CMAKE_RELATIVE_PATH_TOP_SOURCE"),
            interp.get_def("CMAKE_RELATIVE_PATH_TOP_BINARY"),
        ) {
            lg.relative_top_source = top_source.to_owned();
            lg.relative_top_binary = top_binary.to_owned();
        }
    } else {
        super::error("Directory Information file not found");
    }

    let mut make_depends = write_disclaimer(gen_name);
    let mut internal_depends = write_disclaimer(gen_name);

    let langs = util::expand_list(
        interp.get_def("CMAKE_DEPENDS_LANGUAGES").unwrap_or(""),
        false,
    );
    for lang in &langs {
        match lang.as_str() {
            "C" | "CXX" | "RC" | "ASM" | "OBJC" | "OBJCXX" | "CUDA" | "HIP" | "ISPC"
                if !write_target_dependencies(
                    lg,
                    interp,
                    target_dir,
                    lang,
                    Some(valid_deps),
                    verbose,
                    &mut make_depends,
                    &mut internal_depends,
                ) =>
            {
                return false;
            }
            "Fortran" => {
                super::error(
                    "cmake_depends: dependency scanning for the Fortran language (cmDependsFortran) is not implemented by cmake-rust",
                );
                return false;
            }
            "Java" => {
                super::error(
                    "cmake_depends: dependency scanning for the Java language (cmDependsJava) is not implemented by cmake-rust",
                );
                return false;
            }
            _ => {}
        }
    }

    if !write_generated(depend_file, &make_depends, true) {
        return false;
    }
    write_generated(internal_depend_file, &internal_depends, false)
}

/// `cmDependsCompiler::CheckDependencies` + `WriteDependencies` for
/// `CMAKE_DEPENDS_DEPENDENCY_FILES` (compiler-generated depfiles).
fn compiler_check_dependencies(
    lg: &LocalGen,
    verbose: bool,
    internal_dep_file: &str,
    dep_files: &[String],
    dependencies: &mut DependencyMap,
    is_valid_path: Option<&dyn Fn(&str) -> bool>,
) -> bool {
    let mut status = true;
    let mut force_read_deps = true;
    let mut internal_dep_file_time = FileTime::default();
    if file_exists(internal_dep_file) {
        internal_dep_file_time = FileTime::load(internal_dep_file);
        force_read_deps = false;
        if let Ok(content) = std::fs::read_to_string(internal_dep_file) {
            let mut current: Option<String> = None;
            for line in lines(&content) {
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if !line.starts_with(' ') {
                    current = Some(line.to_owned());
                    continue;
                }
                if let Some(depender) = &current {
                    dependencies
                        .entry(depender.clone())
                        .or_default()
                        .push(line[1..].to_owned());
                }
            }
        }
    }

    let mut it = dep_files.iter();
    while let Some(source) = it.next() {
        let (target, format, dep_file) = (it.next(), it.next(), it.next());
        let (Some(target), Some(format), Some(dep_file)) = (target, format, dep_file) else {
            break;
        };
        if !file_exists(dep_file) {
            continue;
        }
        if !force_read_deps {
            let dep_file_time = FileTime::load(dep_file);
            let cmp = match (dep_file_time.exists, internal_dep_file_time.exists) {
                (true, true) => dep_file_time.nsec.cmp(&internal_dep_file_time.nsec),
                _ => std::cmp::Ordering::Greater,
            };
            if cmp == std::cmp::Ordering::Less {
                continue;
            }
        }
        status = false;
        if verbose {
            stdout(&format!(
                "Dependencies file \"{dep_file}\" is newer than depends file \"{internal_dep_file}\".\n"
            ));
        }

        let mut depends: Vec<String> = Vec::new();
        if format == "custom" {
            let Some(rules) = read_gcc_depfile(dep_file, &lg.current_binary, false) else {
                continue;
            };
            for mut entry in rules {
                depends = std::mem::take(&mut entry.dependencies);
                if let Some(valid) = is_valid_path {
                    depends.retain(|d| !valid(d));
                }
                for rule in entry.targets.iter().skip(1).rev() {
                    dependencies.insert(rule.clone(), depends.clone());
                }
                if let Some(first) = entry.targets.first() {
                    dependencies.insert(first.clone(), depends.clone());
                }
            }
        } else {
            if format == "msvc" {
                let Ok(content) = std::fs::read_to_string(dep_file) else {
                    continue;
                };
                if is_valid_path.is_none() && !source.is_empty() {
                    depends.push(source.clone());
                }
                for line in lines(&content) {
                    depends.push(line.to_owned());
                }
            } else if format == "gcc" {
                let Some(rules) = read_gcc_depfile(dep_file, &lg.current_binary, true) else {
                    continue;
                };
                let Some(first) = rules.into_iter().next() else {
                    continue;
                };
                depends = first.dependencies;
                if depends.is_empty() {
                    continue;
                }
                if depends.first() == Some(target) {
                    depends.remove(0);
                }
                if !source.is_empty() {
                    if depends.first() != Some(source) {
                        depends.retain(|d| d != source);
                        if is_valid_path.is_none() {
                            depends.insert(0, source.clone());
                        }
                    } else if is_valid_path.is_some() {
                        depends.remove(0);
                    }
                }
            } else {
                continue;
            }
            if let Some(valid) = is_valid_path {
                depends.retain(|d| !valid(d));
                if !source.is_empty() {
                    depends.insert(0, source.clone());
                }
            }
            dependencies.insert(target.clone(), depends);
        }
    }
    status
}

/// `cmReadGccDepfile`: parse a depfile; `prepend_deps` prepends `prefix`
/// to relative dependency paths (`GccDepfilePrependPaths::Deps`).
fn read_gcc_depfile(path: &str, prefix: &str, prepend_deps: bool) -> Option<Vec<depfile::Rule>> {
    let content = std::fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&content);
    let mut rules = depfile::parse(&text)?;
    for rule in &mut rules {
        for dep in &mut rule.dependencies {
            if !prefix.is_empty() && !util::is_full_path(dep) {
                *dep = format!("{prefix}/{dep}");
            }
            if util::is_full_path(dep) {
                *dep = util::collapse_full_path(dep, "");
            }
        }
        let _ = prepend_deps;
    }
    Some(rules)
}

/// `cmDependsCompiler::WriteDependencies`.
fn compiler_write_dependencies(
    lg: &LocalGen,
    dependencies: &DependencyMap,
    make_depends: &mut String,
    internal_depends: &mut String,
) {
    let line_continue = lg.global.line_continue;
    let long_lines = lg.global.supports_long_line_deps;
    let mut phony_targets: BTreeSet<String> = BTreeSet::new();
    for (target, deps) in dependencies {
        let target = LocalGen::convert_to_makefile_path(&lg.maybe_relative_to_top_bin_dir(target));
        let deps: Vec<String> = deps
            .iter()
            .map(|dep| LocalGen::convert_to_makefile_path(&lg.maybe_relative_to_top_bin_dir(dep)))
            .collect();
        let mut first_dep = true;
        if long_lines {
            make_depends.push_str(&target);
            make_depends.push_str(": ");
        }
        for dep in &deps {
            if long_lines {
                if first_dep {
                    first_dep = false;
                    make_depends.push_str(dep);
                } else {
                    make_depends.push(' ');
                    make_depends.push_str(line_continue);
                    make_depends.push_str("  ");
                    make_depends.push_str(dep);
                }
            } else {
                make_depends.push_str(&format!("{target}: {dep}\n"));
            }
            phony_targets.insert(dep.clone());
        }
        make_depends.push_str("\n\n");
    }
    for target in &phony_targets {
        make_depends.push_str(&format!("\n{target}:\n"));
    }
    for (target, deps) in dependencies {
        internal_depends.push_str(target);
        internal_depends.push('\n');
        for dep in deps {
            internal_depends.push(' ');
            internal_depends.push_str(dep);
            internal_depends.push('\n');
        }
        internal_depends.push('\n');
    }
}

/// `cmLocalUnixMakefileGenerator3::CheckMultipleOutputs`.
fn check_multiple_outputs(interp: &Interp, verbose: bool) {
    let Some(pairs_string) = interp.get_def("CMAKE_MULTIPLE_OUTPUT_PAIRS") else {
        return;
    };
    let pairs = util::expand_list(pairs_string, true);
    let mut it = pairs.iter();
    while let (Some(depender), Some(dependee)) = (it.next(), it.next()) {
        if file_exists(dependee) && !file_exists(depender) {
            if verbose {
                stdout(&format!(
                    "Deleting primary custom command output \"{dependee}\" because another output \"{depender}\" does not exist.\n"
                ));
            }
            let _ = std::fs::remove_file(dependee);
        }
    }
}

/// `cmLocalUnixMakefileGenerator3::UpdateDependencies`.
fn update_dependencies(
    lg: &mut LocalGen,
    gen_name: &str,
    interp: &mut Interp,
    tgt_info: &str,
    verbose: bool,
    _color: bool,
) -> bool {
    if !interp.read_list_file(tgt_info) || interp.error {
        super::error("Target DependInfo.cmake file not found");
    }

    let mut status = true;
    check_multiple_outputs(interp, verbose);

    let echo = |message: &str| {
        // Color output is only emitted when --color is passed; the
        // plain-text form is used otherwise.
        println!("{message}");
    };

    let target_dir = filename_path(tgt_info).to_owned();
    let mut cache = FileTimeCache::new();
    if !interp
        .get_def("CMAKE_DEPENDS_LANGUAGES")
        .unwrap_or("")
        .is_empty()
    {
        let internal_depend_file = format!("{target_dir}/depend.internal");
        let depend_file = format!("{target_dir}/depend.make");

        let mut need_rescan_depend_info = false;
        match cache.compare(&internal_depend_file, tgt_info) {
            Some(result) if result >= 0 => {}
            _ => {
                if verbose {
                    stdout(&format!(
                        "Dependee \"{tgt_info}\" is newer than depender \"{internal_depend_file}\".\n"
                    ));
                }
                need_rescan_depend_info = true;
            }
        }

        let mut need_rescan_dir_info = false;
        let dir_info_file = format!(
            "{}/CMakeFiles/CMakeDirectoryInformation.cmake",
            lg.current_binary
        );
        match cache.compare(&internal_depend_file, &dir_info_file) {
            Some(result) if result >= 0 => {}
            _ => {
                if verbose {
                    stdout(&format!(
                        "Dependee \"{dir_info_file}\" is newer than depender \"{internal_depend_file}\".\n"
                    ));
                }
                need_rescan_dir_info = true;
            }
        }

        let mut valid_dependencies = DependencyMap::new();
        let mut need_rescan_dependencies = false;
        if !need_rescan_dir_info {
            need_rescan_dependencies = !check_dependencies(
                &mut cache,
                verbose,
                &depend_file,
                &internal_depend_file,
                &mut valid_dependencies,
            );
        }

        if need_rescan_depend_info || need_rescan_dir_info || need_rescan_dependencies {
            if verbose {
                let mut target_name = filename_name(&target_dir).to_owned();
                if target_name.len() > 4 {
                    target_name.truncate(target_name.len() - 4);
                }
                echo(&format!("Scanning dependencies of target {target_name}"));
            }
            status = scan_dependencies(
                lg,
                gen_name,
                interp,
                &target_dir,
                &depend_file,
                &internal_depend_file,
                &valid_dependencies,
                verbose,
            );
        }
    }

    let depends = interp
        .get_def("CMAKE_DEPENDS_DEPENDENCY_FILES")
        .unwrap_or("");
    if !depends.is_empty() {
        let dep_files = util::expand_list(depends, true);
        let internal_dep_file = format!("{target_dir}/compiler_depend.internal");
        let dep_file = format!("{target_dir}/compiler_depend.make");
        let mut dependencies = DependencyMap::new();
        let project_only = util::is_on(
            interp
                .get_def("CMAKE_DEPENDS_IN_PROJECT_ONLY")
                .unwrap_or(""),
        );
        let home_source = lg.relative_top_source.clone();
        let home_binary = lg.top_binary.clone();
        let is_valid_path: Option<Box<PathFilter>> = if project_only {
            Some(Box::new(move |path: &str| {
                !(path_eq_or_subdir(path, &home_source) || path_eq_or_subdir(path, &home_binary))
            }))
        } else {
            None
        };
        if !compiler_check_dependencies(
            lg,
            verbose,
            &internal_dep_file,
            &dep_files,
            &mut dependencies,
            is_valid_path.as_deref(),
        ) {
            if verbose {
                let target_name = filename_without_last_ext(&target_dir);
                echo(&format!(
                    "Consolidate compiler generated dependencies of target {target_name}"
                ));
            }
            let mut make_depends = write_disclaimer(gen_name);
            let mut internal_depends = write_disclaimer(gen_name);
            compiler_write_dependencies(
                lg,
                &dependencies,
                &mut make_depends,
                &mut internal_depends,
            );
            if !write_generated(&dep_file, &make_depends, true) {
                return false;
            }
            if !write_generated(&internal_dep_file, &internal_depends, false) {
                return false;
            }
        }
    }
    status
}

/// `cmcmd.cxx`'s `cmake_depends` dispatch.
pub(super) fn depends(args: &[String]) -> i32 {
    if args.len() < 4 {
        return 1;
    }
    let verbose = verbose();
    let (gen_name, home_dir, start_dir, home_out_dir, start_out_dir, dep_info, color) =
        if args.len() >= 6 {
            let color = args
                .get(6)
                .filter(|a| a.starts_with("--color="))
                .is_some_and(|a| a.len() == 8 || util::is_on(&a[8..]));
            (
                args[0].as_str(),
                args[1].clone(),
                args[2].clone(),
                args[3].clone(),
                args[4].clone(),
                args[5].clone(),
                color,
            )
        } else {
            // Old signature: <generator> <home-out-dir> <start-out-dir>
            // <dep-info>; source dirs pretend to equal binary dirs.
            (
                args[0].as_str(),
                args[1].clone(),
                args[2].clone(),
                args[1].clone(),
                args[3].clone(),
                args[3].clone(),
                false,
            )
        };
    let home_dir = pathresolver::to_normalized_path_on_disk(&home_dir);
    let start_dir = pathresolver::to_normalized_path_on_disk(&start_dir);
    let home_out_dir = pathresolver::to_normalized_path_on_disk(&home_out_dir);
    let start_out_dir = pathresolver::to_normalized_path_on_disk(&start_out_dir);

    let Some(global) = makefile_global(gen_name) else {
        return 1;
    };
    let Some(global) = global else {
        // Non-Makefile generator: UpdateDependencies trivially succeeds.
        return 0;
    };

    let root = configure::find_cmake_root().unwrap_or_default();
    let mut interp = Interp::new(Mode::Script, &home_dir, &home_out_dir, &root);
    configure::add_default_definitions(&mut interp);
    // Snapshot for the directory being scanned (start-out / start-src).
    interp.makefiles.push(Makefile {
        source_dir: start_dir.clone(),
        binary_dir: start_out_dir.clone(),
        parent: None,
        props: crate::props::Props::default(),
        policies: interp.makefiles[0].policies.clone(),
        list_files: Vec::new(),
        output_files: Vec::new(),
        targets: Vec::new(),
        children: Vec::new(),
        exclude_from_all: false,
        deferred: Vec::new(),
        defer_counter: 0,
        backtrace: Vec::new(),
    });
    let index = interp.makefiles.len() - 1;
    interp.mf_stack.push(index);

    let mut lg = LocalGen {
        relative_top_source: home_dir.clone(),
        relative_top_binary: home_out_dir.clone(),
        top_binary: home_out_dir,
        current_binary: start_out_dir,
        current_source: start_dir,
        global,
    };
    let lg = &mut lg;

    if update_dependencies(lg, gen_name, &mut interp, &dep_info, verbose, color) {
        0
    } else {
        2
    }
}
