//! kwsys `SystemTools` file copying, step for step: `CopyFileAlways`,
//! `CopyFileIfDifferent`, `CopyAFile`, `CopyADirectory`, `FilesDiffer` and
//! `MakeDirectory`.  `cmake -E copy*` and `file(CREATE_LINK COPY_ON_ERROR)`
//! use these; their quirks (a directory source creating a directory, the
//! destination being removed before it is rewritten, a directory listing
//! taken before the destination exists) are part of the observable
//! behaviour.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;

use crate::util;

/// `SystemTools::FileIsDirectory` (follows symlinks).
pub fn is_dir(path: &str) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_dir())
}

/// `SystemTools::MakeDirectory`: an existing directory is success, an
/// existing non-directory is EEXIST; parents are created as needed.
pub fn make_directory(path: &str) -> io::Result<()> {
    if path.is_empty() {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    if fs::symlink_metadata(path).is_ok() || fs::metadata(path).is_ok() {
        if is_dir(path) {
            return Ok(());
        }
        return Err(io::Error::from_raw_os_error(libc::EEXIST));
    }
    let dir = path.replace('\\', "/");
    for (pos, _) in dir.match_indices('/') {
        if pos > 0 {
            let _ = fs::create_dir(&dir[..pos]);
        }
    }
    match fs::create_dir(&dir) {
        Ok(()) => Ok(()),
        Err(error) if error.raw_os_error() == Some(libc::EEXIST) => Ok(()),
        Err(error) => Err(error),
    }
}

/// `SystemTools::SameFile`: both exist and share device and inode.
fn same_file(a: &str, b: &str) -> bool {
    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// `SystemTools::FilesDiffer`.
pub fn files_differ(source: &str, destination: &str) -> bool {
    let (Ok(src_meta), Ok(dst_meta)) = (fs::metadata(source), fs::metadata(destination)) else {
        return true;
    };
    if src_meta.len() != dst_meta.len() {
        return true;
    }
    if src_meta.len() == 0 {
        return false;
    }
    let (Ok(mut a), Ok(mut b)) = (File::open(source), File::open(destination)) else {
        return true;
    };
    let mut left = src_meta.len();
    let mut buf_a = [0u8; 4096];
    let mut buf_b = [0u8; 4096];
    while left > 0 {
        let next = left.min(4096) as usize;
        if a.read_exact(&mut buf_a[..next]).is_err() || b.read_exact(&mut buf_b[..next]).is_err() {
            return true;
        }
        if buf_a[..next] != buf_b[..next] {
            return true;
        }
        left -= next as u64;
    }
    false
}

/// `SystemTools::CopyFileContentBlockwise` (after `CloneFileContent`
/// fails): the destination entry is removed first, and a read error ends
/// the copy without failing it.
fn copy_content(source: &str, destination: &str) -> io::Result<()> {
    let mut input = File::open(source)?;
    let _ = fs::remove_file(destination);
    let mut output = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(destination)?;
    let mut buffer = [0u8; 4096];
    loop {
        match input.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(n) => output.write_all(&buffer[..n])?,
        }
    }
    output.flush()
}

/// `SystemTools::CopyFileAlways`.
pub fn copy_file_always(source: &str, destination: &str) -> io::Result<()> {
    let permissions = fs::metadata(source).ok().map(|m| m.permissions());
    let mut real_destination = destination.to_owned();
    if is_dir(source) {
        make_directory(destination)?;
    } else {
        let destination_dir = if is_dir(destination) {
            real_destination = format!(
                "{}/{}",
                crate::commands::install::unix_slashes(destination),
                util::filename_name(source)
            );
            destination.to_owned()
        } else {
            util::filename_path(destination)
        };
        if same_file(source, &real_destination) {
            return Ok(());
        }
        if !destination_dir.is_empty() {
            make_directory(&destination_dir)?;
        }
        copy_content(source, &real_destination)?;
    }
    if let Some(permissions) = permissions {
        fs::set_permissions(&real_destination, permissions)?;
    }
    Ok(())
}

/// `SystemTools::CopyFileIfDifferent`.
pub fn copy_file_if_different(source: &str, destination: &str) -> io::Result<()> {
    if is_dir(destination) {
        let new_destination = format!("{destination}/{}", util::filename_name(source));
        if new_destination != destination {
            return copy_file_if_different(source, &new_destination);
        }
    } else if files_differ(source, destination) {
        return copy_file_always(source, destination);
    }
    Ok(())
}

/// `SystemTools::CopyAFile`.
pub fn copy_a_file(source: &str, destination: &str, always: bool) -> io::Result<()> {
    if always {
        copy_file_always(source, destination)
    } else {
        copy_file_if_different(source, destination)
    }
}

/// `SystemTools::CopyADirectory`: the source listing is loaded before the
/// destination is created, then each entry is copied, recursing into
/// directories (symlinked ones included).
pub fn copy_a_directory(source: &str, destination: &str, always: bool) -> io::Result<()> {
    let entries: Vec<String> = fs::read_dir(source)?
        .map(|entry| entry.map(|entry| crate::bytes::decode_os(&entry.file_name())))
        .collect::<io::Result<_>>()?;
    make_directory(destination)?;
    for name in entries {
        let full = format!("{source}/{name}");
        if is_dir(&full) {
            copy_a_directory(&full, &format!("{destination}/{name}"), always)?;
        } else {
            copy_a_file(&full, destination, always)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn make_directory_rejects_files() {
        let dir = std::env::temp_dir().join(format!("kwsys-copy-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("f");
        fs::write(&file, "x").unwrap();
        let file = file.to_string_lossy().into_owned();
        assert_eq!(
            make_directory(&file).unwrap_err().raw_os_error(),
            Some(libc::EEXIST)
        );
        assert!(make_directory(&format!("{}/a/b", dir.display())).is_ok());
        let _ = fs::remove_dir_all(&dir);
    }
}
