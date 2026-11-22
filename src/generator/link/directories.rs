//! Search paths ordered by the files that must be found in each directory.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use super::*;
use crate::util::{filename_name, filename_path};

pub(super) struct Directories {
    users: Vec<String>,
    languages: Vec<String>,
    constraints: Vec<Constraint>,
    known: HashSet<(String, bool)>,
}

struct Constraint {
    path: String,
    directory: String,
    name: String,
    kind: ConstraintKind,
}

enum ConstraintKind {
    Runtime(Option<String>),
    Link(Vec<String>),
}

impl Directories {
    pub fn new(users: Vec<String>) -> Self {
        Self {
            users,
            languages: Vec::new(),
            constraints: Vec::new(),
            known: HashSet::new(),
        }
    }

    pub fn add_language(&mut self, dirs: Vec<String>) {
        self.languages.extend(dirs);
    }

    pub fn runtime(&mut self, path: &str, soname: Option<String>) {
        self.add(
            path,
            ConstraintKind::Runtime(soname.or_else(|| elf_soname(path))),
        );
    }

    pub fn library(&mut self, path: &str, alternatives: Vec<String>) {
        self.add(path, ConstraintKind::Link(alternatives));
    }

    fn add(&mut self, path: &str, kind: ConstraintKind) {
        let directory = filename_path(path);
        let name = filename_name(path);
        if self
            .known
            .insert((path.to_owned(), matches!(kind, ConstraintKind::Runtime(_))))
        {
            self.constraints.push(Constraint {
                path: path.to_owned(),
                directory,
                name,
                kind,
            });
        }
    }

    pub fn ordered(&self, implicit: &HashSet<String>, artifacts: &HashSet<String>) -> Vec<String> {
        // Compare real paths so symlink aliases of implicit directories stay implicit.
        let real = |dir: &str| -> String {
            fs::canonicalize(dir)
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_else(|_| dir.to_owned())
        };
        let implicit_real: HashSet<String> = implicit.iter().map(|dir| real(dir)).collect();
        let dirs = unique(
            self.users
                .iter()
                .chain(self.constraints.iter().map(|c| &c.directory))
                .chain(&self.languages)
                .filter(|dir| !implicit_real.contains(&real(dir)))
                .cloned(),
        );
        let mut edges = vec![BTreeSet::new(); dirs.len()];
        let real_paths = dirs.iter().map(fs::canonicalize).collect::<Vec<_>>();
        for c in &self.constraints {
            let Some(index) = dirs.iter().position(|dir| dir == &c.directory) else {
                continue;
            };
            for (i, dir) in dirs.iter().enumerate() {
                if index != i
                    && !matches!((&real_paths[index], &real_paths[i]), (Ok(a), Ok(b)) if a == b)
                    && c.conflicts(dir, artifacts)
                {
                    edges[i].insert(index);
                }
            }
        }
        let edges = edges
            .into_iter()
            .map(|edge| edge.into_iter().collect())
            .collect::<Vec<_>>();
        dependencies::postorder(&edges, 0..dirs.len())
            .into_iter()
            .map(|i| dirs[i].clone())
            .collect()
    }
}

impl Constraint {
    fn conflicts(&self, directory: &str, artifacts: &HashSet<String>) -> bool {
        match &self.kind {
            ConstraintKind::Runtime(Some(name)) => {
                self.conflicting_file(directory, name, artifacts)
            }
            ConstraintKind::Runtime(None) => {
                fs::read_dir(directory)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .any(|entry| entry.file_name().to_string_lossy().starts_with(&self.name))
                    || artifacts.iter().any(|file| {
                        filename_path(file) == directory
                            && filename_name(file).starts_with(&self.name)
                    })
            }
            ConstraintKind::Link(alternatives) => std::iter::once(&self.name)
                .chain(alternatives)
                .any(|name| self.conflicting_file(directory, name, artifacts)),
        }
    }

    fn conflicting_file(&self, dir: &str, name: &str, artifacts: &HashSet<String>) -> bool {
        let file = format!("{dir}/{name}");
        match (fs::metadata(&file), fs::metadata(&self.path)) {
            (Ok(candidate), Ok(original)) => {
                candidate.is_file()
                    && (candidate.ino(), candidate.dev()) != (original.ino(), original.dev())
            }
            (Ok(candidate), Err(_)) => candidate.is_file(),
            (Err(_), _) => artifacts.contains(&file),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_conflicts_reorder_search_directories() {
        let mut directories = Directories::new(vec![
            "/probe/user".into(),
            "/probe/other".into(),
            "/probe/implicit".into(),
        ]);
        directories.runtime("/probe/runtime/liba.so.1.0", Some("liba.so.1".into()));
        directories.library("/probe/link/libb.so", vec!["libb.a".into()]);
        directories.add_language(vec!["/probe/language".into()]);
        let implicit = HashSet::from(["/probe/implicit".to_owned()]);
        let artifacts = HashSet::from([
            "/probe/user/liba.so.1".to_owned(),
            "/probe/user/libb.a".to_owned(),
        ]);
        assert_eq!(
            directories.ordered(&implicit, &artifacts),
            [
                "/probe/runtime",
                "/probe/link",
                "/probe/user",
                "/probe/other",
                "/probe/language",
            ]
        );
    }
}
