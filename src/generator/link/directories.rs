//! Port of `cmOrderDirectories`: search paths ordered by the files that must
//! be found in each directory.

use std::cell::OnceCell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use super::*;
use crate::cmregex::Regex;
use crate::util::{filename_name, filename_path};

/// `cmGlobalGenerator::DirectoryContentMap`: the artifacts that the build
/// will create in each directory, and what was found on disk.
#[derive(Default)]
pub(in crate::generator) struct DirectoryContents {
    generated: HashMap<String, BTreeSet<String>>,
    /// Directories whose disk content was loaded.
    all: HashMap<String, BTreeSet<String>>,
}

impl DirectoryContents {
    /// `cmGlobalGenerator::AddToManifest`.
    pub fn add(&mut self, directory: &str, file: String) {
        if !file.is_empty() {
            self.generated
                .entry(directory.to_owned())
                .or_default()
                .insert(file);
        }
    }

    /// `cmGlobalGenerator::GetDirectoryContent`.
    fn content(&mut self, directory: &str, need_disk: bool) -> &BTreeSet<String> {
        if need_disk && !self.all.contains_key(directory) {
            let mut files = self.generated.get(directory).cloned().unwrap_or_default();
            files.extend(
                fs::read_dir(directory)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned()),
            );
            self.all.insert(directory.to_owned(), files);
        }
        if self.all.contains_key(directory) {
            return &self.all[directory];
        }
        self.generated.entry(directory.to_owned()).or_default()
    }
}

/// Where the warnings about `target` go, and what the build will create.
pub(super) struct Reporter<'a, 'q, 'i> {
    pub query: &'a Query<'q, 'i>,
    pub target: &'a Target,
}

pub(super) struct OrderDirectories {
    purpose: &'static str,
    users: Vec<String>,
    languages: Vec<String>,
    constraints: Vec<Constraint>,
    implicit_entries: Vec<Constraint>,
    implicit: HashSet<String>,
    emitted_runtime: HashSet<String>,
    emitted_library: HashSet<String>,
    link_extensions: Vec<String>,
    remove_extension: Option<Regex>,
    ordered: OnceCell<Vec<String>>,
}

struct Constraint {
    path: String,
    directory: String,
    name: String,
    kind: ConstraintKind,
}

enum ConstraintKind {
    /// The soname, if known.
    Runtime(Option<String>),
    Library,
}

/// `^(.*)/(.*).framework/(.*)$` with a binary named after the framework: the
/// directory containing the framework.
fn framework_parent(path: &str) -> Option<String> {
    thread_local! {
        static SPLIT: Regex = Regex::new("^(.*)/(.*).framework/(.*)$").unwrap();
    }
    if !path.contains(".framework") {
        return None;
    }
    let found = SPLIT.with(|regex| regex.find(path))?;
    let group = |index: usize| found.text(path, index).unwrap_or_default().into_owned();
    group(3).contains(&group(2)).then(|| group(1))
}

impl Constraint {
    fn new(path: &str, kind: ConstraintKind) -> Constraint {
        let (directory, name) = framework_parent(path)
            .map(|parent| {
                let name = path[parent.len() + 1..].to_owned();
                (parent, name)
            })
            .filter(|(_, name)| !name.is_empty())
            .unwrap_or_else(|| (filename_path(path), filename_name(path)));
        Constraint {
            path: path.to_owned(),
            directory,
            name,
            kind,
        }
    }

    fn report(&self) -> String {
        match &self.kind {
            ConstraintKind::Runtime(soname) => {
                format!(
                    "runtime library [{}]",
                    soname.as_ref().unwrap_or(&self.name)
                )
            }
            ConstraintKind::Library => format!("link library [{}]", self.name),
        }
    }

    fn conflicts(
        &self,
        order: &OrderDirectories,
        directory: &str,
        contents: &mut DirectoryContents,
    ) -> bool {
        match &self.kind {
            ConstraintKind::Runtime(Some(name)) => self.file_conflicts(directory, name, contents),
            ConstraintKind::Runtime(None) => {
                // We do not have the soname. Look at all files that start
                // with the file name.
                let Some(last) = self.name.chars().next_back() else {
                    return false;
                };
                let mut limit = self.name[..self.name.len() - last.len_utf8()].to_owned();
                limit.push(char::from_u32(last as u32 + 1).unwrap_or(last));
                contents
                    .content(directory, true)
                    .range(self.name.clone()..=limit)
                    .next()
                    .is_some()
            }
            ConstraintKind::Library => {
                if self.file_conflicts(directory, &self.name, contents) {
                    return true;
                }
                // Now check if the file exists with other extensions the
                // linker might consider.
                let Some(found) = order
                    .remove_extension
                    .as_ref()
                    .filter(|_| !order.link_extensions.is_empty())
                    .and_then(|regex| regex.find(&self.name))
                else {
                    return false;
                };
                let group = |index: usize| found.text(&self.name, index).unwrap_or_default();
                order.link_extensions.iter().any(|extension| {
                    *extension != group(2)
                        && self.file_conflicts(
                            directory,
                            &format!("{}{extension}", group(1)),
                            contents,
                        )
                })
            }
        }
    }

    /// `FileMayConflict`.
    fn file_conflicts(&self, dir: &str, name: &str, contents: &mut DirectoryContents) -> bool {
        let file = format!("{dir}/{name}");
        if let Ok(candidate) = fs::metadata(&file)
            && !candidate.is_dir()
        {
            // The file conflicts only if it is not the same as the original
            // file due to a symlink or hardlink.
            return !fs::metadata(&self.path).is_ok_and(|original| {
                (candidate.ino(), candidate.dev()) == (original.ino(), original.dev())
            });
        }
        // Check if the file will be built by cmake.
        contents.content(dir, false).contains(name)
    }
}

fn real_path(dir: &str) -> String {
    fs::canonicalize(dir).map_or_else(
        |_| dir.to_owned(),
        |path| path.to_string_lossy().into_owned(),
    )
}

impl OrderDirectories {
    pub fn new(purpose: &'static str, implicit: &BTreeSet<String>) -> Self {
        Self {
            purpose,
            users: Vec::new(),
            languages: Vec::new(),
            constraints: Vec::new(),
            implicit_entries: Vec::new(),
            implicit: implicit.iter().map(|dir| real_path(dir)).collect(),
            emitted_runtime: HashSet::new(),
            emitted_library: HashSet::new(),
            link_extensions: Vec::new(),
            remove_extension: None,
            ordered: OnceCell::new(),
        }
    }

    pub fn add_users(&mut self, dirs: Vec<String>) {
        self.users.extend(dirs);
    }

    pub fn add_languages(&mut self, dirs: Vec<String>) {
        self.languages.extend(dirs);
    }

    pub fn set_link_extensions(&mut self, extensions: Vec<String>, remove: Option<Regex>) {
        self.link_extensions = extensions;
        self.remove_extension = remove;
    }

    /// `AddRuntimeLibrary`.
    pub fn add_runtime(&mut self, path: &str, soname: Option<String>) {
        if !self.emitted_runtime.insert(path.to_owned()) {
            return;
        }
        let implicit = !self.implicit.is_empty()
            && self.is_implicit(&framework_parent(path).unwrap_or_else(|| filename_path(path)));
        let soname = soname.or_else(|| elf_soname(path));
        self.insert(path, ConstraintKind::Runtime(soname), implicit);
    }

    /// `AddLinkLibrary`.
    pub fn add_library(&mut self, path: &str) {
        if self.emitted_library.insert(path.to_owned()) {
            let implicit = !self.implicit.is_empty() && self.is_implicit(&filename_path(path));
            self.insert(path, ConstraintKind::Library, implicit);
        }
    }

    fn insert(&mut self, path: &str, kind: ConstraintKind, implicit: bool) {
        let constraint = Constraint::new(path, kind);
        if implicit {
            self.implicit_entries.push(constraint);
        } else {
            self.constraints.push(constraint);
        }
    }

    fn is_implicit(&self, dir: &str) -> bool {
        self.implicit.contains(&real_path(dir))
    }

    /// `GetOrderedDirectories`.
    pub fn ordered(&self, reporter: &Reporter<'_, '_, '_>) -> &[String] {
        self.ordered.get_or_init(|| self.order(reporter))
    }

    fn order(&self, reporter: &Reporter<'_, '_, '_>) -> Vec<String> {
        let mut dirs: Vec<String> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        let add = |dirs: &mut Vec<String>, index: &mut HashMap<String, usize>, dir: &str| {
            *index.entry(dir.to_owned()).or_insert_with(|| {
                dirs.push(dir.to_owned());
                dirs.len() - 1
            })
        };
        let explicit = |dir: &&String| !self.is_implicit(dir) && !dir.is_empty();
        for dir in self.users.iter().filter(explicit) {
            add(&mut dirs, &mut index, dir);
        }
        let owners: Vec<usize> = self
            .constraints
            .iter()
            .map(|constraint| add(&mut dirs, &mut index, &constraint.directory))
            .collect();
        for dir in self.languages.iter().filter(explicit) {
            add(&mut dirs, &mut index, dir);
        }
        if dirs.is_empty() {
            return dirs;
        }

        let mut conflicts: Vec<Vec<(usize, usize)>> = vec![Vec::new(); dirs.len()];
        let contents = reporter.query.generator.directory_contents(reporter.query);
        let mut contents = contents.borrow_mut();
        if dirs.len() > 1 {
            let real: Vec<String> = dirs.iter().map(|dir| real_path(dir)).collect();
            for (entry, constraint) in self.constraints.iter().enumerate() {
                let owner = owners[entry];
                for (i, dir) in dirs.iter().enumerate() {
                    if real[i] != real[owner] && constraint.conflicts(self, dir, &mut contents) {
                        // The library will be found in this directory but
                        // this is not the directory named for it. Make sure
                        // the desired directory comes before this one.
                        conflicts[i].push((owner, entry));
                    }
                }
            }
        }
        for list in &mut conflicts {
            list.sort_unstable();
            list.dedup_by_key(|(owner, _)| *owner);
        }
        self.find_implicit_conflicts(&dirs, &mut contents, reporter);

        let mut walk = Walk {
            order: self,
            reporter,
            dirs: &dirs,
            conflicts: &conflicts,
            visited: vec![0; dirs.len()],
            id: 0,
            diagnosed: false,
            result: Vec::new(),
        };
        for i in 0..dirs.len() {
            walk.id += 1;
            walk.visit(i);
        }
        walk.result
    }

    fn find_implicit_conflicts(
        &self,
        dirs: &[String],
        contents: &mut DirectoryContents,
        reporter: &Reporter<'_, '_, '_>,
    ) {
        let mut text = String::new();
        for entry in &self.implicit_entries {
            let mut first = true;
            for dir in dirs {
                if *dir != entry.directory
                    && real_path(dir) != real_path(&entry.directory)
                    && entry.conflicts(self, dir, contents)
                {
                    if first {
                        first = false;
                        text.push_str(&format!(
                            "  {} in {} may be hidden by files in:\n",
                            entry.report(),
                            entry.directory
                        ));
                    }
                    text.push_str(&format!("    {dir}\n"));
                }
            }
        }
        if !text.is_empty() {
            reporter.warning(format!(
                "Cannot generate a safe {} for target {} because files in some directories may conflict with  libraries in implicit directories:\n{text}Some of these libraries may not be found correctly.",
                self.purpose, reporter.target.name
            ));
        }
    }
}

impl Reporter<'_, '_, '_> {
    fn warning(&self, text: String) {
        self.query
            .ctx
            .message(MessageType::Warning, self.target.backtrace.clone(), text);
    }
}

/// The depth-first `VisitDirectory` walk.
struct Walk<'a, 'r, 'q, 'i> {
    order: &'a OrderDirectories,
    reporter: &'a Reporter<'r, 'q, 'i>,
    dirs: &'a [String],
    conflicts: &'a [Vec<(usize, usize)>],
    visited: Vec<usize>,
    id: usize,
    diagnosed: bool,
    result: Vec<String>,
}

impl Walk<'_, '_, '_, '_> {
    fn visit(&mut self, i: usize) {
        if self.visited[i] != 0 {
            if self.visited[i] == self.id {
                self.diagnose_cycle();
            }
            return;
        }
        self.visited[i] = self.id;
        for (next, _) in &self.conflicts[i] {
            self.visit(*next);
        }
        self.result.push(self.dirs[i].clone());
    }

    fn diagnose_cycle(&mut self) {
        if self.diagnosed {
            return;
        }
        self.diagnosed = true;
        let mut text = format!(
            "Cannot generate a safe {} for target {} because there is a cycle in the constraint graph:\n",
            self.order.purpose, self.reporter.target.name
        );
        for (i, list) in self.conflicts.iter().enumerate() {
            text.push_str(&format!("  dir {i} is [{}]\n", self.dirs[i]));
            for (owner, entry) in list {
                text.push_str(&format!(
                    "    dir {owner} must precede it due to {}\n",
                    self.order.constraints[*entry].report()
                ));
            }
        }
        text.push_str("Some of these libraries may not be found correctly.");
        self.reporter.warning(text);
    }
}

/// Read DT_SONAME without invoking a platform tool. Translate DT_STRTAB
/// through a PT_LOAD segment so stripped shared libraries are supported.
pub(super) fn elf_soname(path: &str) -> Option<String> {
    let bytes = if fs::metadata(path).is_ok_and(|metadata| metadata.is_file()) {
        fs::read(path).unwrap_or_default()
    } else {
        Vec::new()
    };
    if bytes.get(..4) != Some(b"\x7fELF") {
        let link = fs::read_link(path).ok()?;
        let name = link.file_name()?.to_str()?;
        let base = Path::new(path).file_name()?.to_str()?;
        return (link.components().count() == 1
            && name.len() > base.len()
            && name.starts_with(base))
        .then(|| name.to_owned());
    }

    let endian = *bytes.get(5)?;
    let wide = match bytes.get(4)? {
        1 => false,
        2 => true,
        _ => return None,
    };
    let read = |offset: usize, size: usize| -> Option<usize> {
        let range = bytes.get(offset..offset.checked_add(size)?)?;
        let value = if endian == 1 {
            range
                .iter()
                .rev()
                .fold(0u64, |value, byte| (value << 8) | u64::from(*byte))
        } else if endian == 2 {
            range
                .iter()
                .fold(0u64, |value, byte| (value << 8) | u64::from(*byte))
        } else {
            return None;
        };
        usize::try_from(value).ok()
    };
    let word = if wide { 8 } else { 4 };
    let (table, size, count) = if wide { (32, 54, 56) } else { (28, 42, 44) };
    let table = read(table, word)?;
    let size = read(size, 2)?;
    let count = read(count, 2)?;
    let mut loads = Vec::new();
    let mut dynamic = None;
    for index in 0..count {
        let start = table.checked_add(index * size)?;
        let header = bytes.get(start..start.checked_add(size)?)?;
        if header.len() < if wide { 56 } else { 32 } {
            return None;
        }
        let (offset, address, length) = if wide { (8, 16, 32) } else { (4, 8, 16) };
        let entry = (
            read(start + offset, word)?,
            read(start + address, word)?,
            read(start + length, word)?,
        );
        match read(start, 4)? {
            1 => loads.push(entry),
            2 => dynamic = Some(entry),
            _ => {}
        }
    }
    let (offset, _, length) = dynamic?;
    bytes.get(offset..offset.checked_add(length)?)?;
    let mut strtab = None;
    let mut soname = None;
    for entry in 0..length / (2 * word) {
        let entry = offset + entry * 2 * word;
        match read(entry, word)? {
            0 => break,
            5 => strtab = read(entry + word, word),
            14 => soname = read(entry + word, word),
            _ => {}
        }
    }
    let address = strtab?.checked_add(soname?)?;
    for (file, start, length) in loads {
        if let Some(relative) = address
            .checked_sub(start)
            .filter(|relative| *relative < length)
        {
            let tail = bytes.get(file.checked_add(relative)?..file.checked_add(length)?)?;
            let end = tail.iter().position(|&byte| byte == 0)?;
            return std::str::from_utf8(&tail[..end]).ok().map(str::to_owned);
        }
    }
    None
}
