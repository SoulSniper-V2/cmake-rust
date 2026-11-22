//! Filesystem helpers.

use std::path::Path;

/// Writes `data` to `path` only when the content differs (like
/// `cmGeneratedFileStream` with copy-if-different).
pub fn write_if_different(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Ok(existing) = std::fs::read(path)
        && existing == data
    {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension("tmp-cmake-rust");
    std::fs::write(&temp, data)?;
    std::fs::rename(&temp, path)
}

/// Writes `data` to `path` unconditionally through a temporary file
/// (`cmGeneratedFileStream` without copy-if-different).
pub fn write_generated(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension("tmp-cmake-rust");
    std::fs::write(&temp, data)?;
    std::fs::rename(&temp, path)
}
