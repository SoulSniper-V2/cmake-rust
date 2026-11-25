//! Filesystem helpers.

use std::path::Path;

/// Writes `data` to `path` only when the content differs (like
/// `cmGeneratedFileStream` with copy-if-different).
pub fn write_if_different(path: &Path, data: &str) -> std::io::Result<()> {
    write_if_different_bytes(path, &crate::bytes::encode(data))
}

/// [`write_if_different`] for bytes that are already external (a copied or
/// configured file's raw content).
pub fn write_if_different_bytes(path: &Path, data: &[u8]) -> std::io::Result<()> {
    generated_file_stream(path, data, true)
}

/// Writes `data` to `path` unconditionally through a temporary file
/// (`cmGeneratedFileStream` without copy-if-different).
pub fn write_generated(path: &Path, data: &str) -> std::io::Result<()> {
    write_generated_bytes(path, &crate::bytes::encode(data))
}

/// Raw-byte form of [`write_generated`] for data that did not pass through a
/// CMake value (for example a template with unsubstituted non-UTF-8 bytes).
pub fn write_generated_bytes(path: &Path, data: &[u8]) -> std::io::Result<()> {
    generated_file_stream(path, data, false)
}

/// `strerror` text of an I/O error (no " (os error N)" suffix).
pub fn strerror(error: &std::io::Error) -> String {
    let text = error.to_string();
    match error.raw_os_error() {
        Some(code) => text
            .strip_suffix(&format!(" (os error {code})"))
            .unwrap_or(&text)
            .to_owned(),
        None => text,
    }
}

/// `cmSystemTools::RandomNumber() & 0xFFFFF` for temporary names.
fn random_suffix() -> u32 {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    hasher.write_u32(std::process::id());
    (hasher.finish() as u32) & 0xFFFFF
}

/// `cmGeneratedFileStream` Open + write + Close: the content goes to
/// `<name>.tmpXXXXX` (its directory created first), which then replaces
/// `name` (always, or only when the files differ). A temporary that cannot
/// be opened is reported like upstream ("Cannot open file for write" and
/// the system error) and returned as the error; a failed rename is ignored
/// as upstream ignores it. The temporary is always removed.
fn generated_file_stream(path: &Path, data: &[u8], copy_if_different: bool) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut temp = path.as_os_str().to_owned();
    temp.push(format!(".tmp{:05x}", random_suffix()));
    let temp = std::path::PathBuf::from(temp);
    let _ = std::fs::remove_file(&temp);
    if let Some(parent) = temp.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut file = match std::fs::File::create(&temp) {
        Ok(file) => file,
        Err(error) => {
            crate::cmcmd::error(format!(
                "Cannot open file for write: {}",
                temp.to_string_lossy()
            ));
            crate::cmcmd::error(format!(": System Error: {}", strerror(&error)));
            return Err(error);
        }
    };
    let okay = file.write_all(data).and_then(|()| file.flush()).is_ok();
    drop(file);
    // cmSystemTools::FilesDiffer: differing sizes or bytes, or unreadable.
    let differ = || match (std::fs::read(&temp), std::fs::read(path)) {
        (Ok(new), Ok(old)) => new != old,
        _ => true,
    };
    if okay && (!copy_if_different || differ()) {
        let _ = std::fs::rename(&temp, path);
    }
    let _ = std::fs::remove_file(&temp);
    Ok(())
}

/// `cmSystemTools::CopyFileIfDifferent` with the source's bytes in hand:
/// nothing happens when the destination already has them; otherwise the
/// destination directory is created and the file written in place.
pub fn copy_if_different_bytes(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Ok(existing) = std::fs::read(path)
        && existing == data
    {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, data)
}
