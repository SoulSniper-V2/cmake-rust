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
pub fn write_generated(path: &Path, data: &str) -> std::io::Result<()> {
    write_generated_bytes(path, &crate::bytes::encode(data))
}

/// Raw-byte form of [`write_generated`] for data that did not pass through a
/// CMake value (for example a template with unsubstituted non-UTF-8 bytes).
pub fn write_generated_bytes(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension("tmp-cmake-rust");
    std::fs::write(&temp, data)?;
    std::fs::rename(&temp, path)
}
