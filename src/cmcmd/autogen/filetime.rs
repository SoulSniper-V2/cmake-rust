//! `cmFileTime` on Unix: nanoseconds since the epoch (`UtPerS = 1e9`).
//! An unloaded/missing file has `Time = 0`, like upstream's default.

use std::time::SystemTime;

#[derive(Clone, Copy, Default)]
pub(crate) struct FileTime {
    /// `Load` succeeded (the file exists and has a readable mtime).
    loaded: bool,
    nsec: i128,
}

impl FileTime {
    /// `cmFileTime::Load` (follows symlinks): true when a readable
    /// modification time exists.
    pub(crate) fn load(path: &str) -> FileTime {
        match std::fs::metadata(path).and_then(|m| m.modified()) {
            Ok(mtime) => {
                let nsec = match mtime.duration_since(SystemTime::UNIX_EPOCH) {
                    Ok(d) => i128::from(d.as_secs()) * 1_000_000_000 + i128::from(d.subsec_nanos()),
                    Err(e) => {
                        let d = e.duration();
                        -(i128::from(d.as_secs()) * 1_000_000_000 + i128::from(d.subsec_nanos()))
                    }
                };
                FileTime { loaded: true, nsec }
            }
            Err(_) => FileTime::default(),
        }
    }

    /// `cmFileTime::Newer`: `(ftm.Time - this->Time) < 0`.
    pub(crate) fn newer(&self, other: &FileTime) -> bool {
        self.nsec > other.nsec
    }

    /// `cmFileTime::Older`: `(this->Time - ftm.Time) < 0`.
    pub(crate) fn older(&self, other: &FileTime) -> bool {
        self.nsec < other.nsec
    }

    /// `cmFileTime::Differ`.
    #[allow(dead_code)]
    pub(crate) fn differ(&self, other: &FileTime) -> bool {
        self.nsec != other.nsec
    }

    /// The file the time was loaded from exists (the `Load` result).
    pub(crate) fn exists(&self) -> bool {
        self.loaded
    }
}
