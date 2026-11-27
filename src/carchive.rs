//! `cmArchiveWrite`, `cmSystemTools::CreateTar`, `ExtractTar` and `ListTar`
//! running on libarchive, the same library upstream CMake links.
//!
//! Like file(DOWNLOAD)'s libcurl loader, the library is dlopen'ed at first
//! use (`libarchive.so.13`), so cmake-rust has no hard dependency on it. If
//! it cannot be loaded, archive commands fail with an explicit error.

use std::ffi::{CStr, CString, c_char, c_int, c_long, c_void};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

type LaArchive = *mut c_void;
type LaEntry = *mut c_void;
type LaSsize = i64;
type LaInt64 = i64;

const ARCHIVE_EOF: c_int = 1;
const ARCHIVE_OK: c_int = 0;
const ARCHIVE_WARN: c_int = -20;
const ARCHIVE_EXTRACT_TIME: c_int = 0x0004;

const AE_IFCHR: c_int = 0o020000;
const AE_IFBLK: c_int = 0o060000;

/// `cmArchiveWrite::Compress` / `cmSystemTools::cmTarCompression`.
/// Upstream exposes `Compress` (`.Z`) and `Lzma` as well; no current caller
/// selects them, so dead-code warnings on them are suppressed.
#[derive(Clone, Copy, PartialEq, Eq)]
#[allow(dead_code, clippy::enum_variant_names)]
pub enum Compress {
    None,
    Compress,
    GZip,
    BZip2,
    Lzma,
    Xz,
    Zstd,
}

/// `cmSystemTools::cmTarExtractTimestamps`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ExtractTimestamps {
    Yes,
    No,
}

struct La {
    write_new: unsafe extern "C" fn() -> LaArchive,
    write_free: unsafe extern "C" fn(LaArchive) -> c_int,
    write_add_filter_none: unsafe extern "C" fn(LaArchive) -> c_int,
    write_add_filter_compress: unsafe extern "C" fn(LaArchive) -> c_int,
    write_add_filter_gzip: unsafe extern "C" fn(LaArchive) -> c_int,
    write_add_filter_bzip2: unsafe extern "C" fn(LaArchive) -> c_int,
    write_add_filter_lzma: unsafe extern "C" fn(LaArchive) -> c_int,
    write_add_filter_xz: unsafe extern "C" fn(LaArchive) -> c_int,
    write_add_filter_zstd: unsafe extern "C" fn(LaArchive) -> c_int,
    write_set_filter_option:
        unsafe extern "C" fn(LaArchive, *const c_char, *const c_char, *const c_char) -> c_int,
    write_set_format_by_name: unsafe extern "C" fn(LaArchive, *const c_char) -> c_int,
    write_set_bytes_in_last_block: unsafe extern "C" fn(LaArchive, c_int) -> c_int,
    write_open: unsafe extern "C" fn(
        LaArchive,
        *mut c_void,
        *mut c_void,
        unsafe extern "C" fn(LaArchive, *mut c_void, *const c_void, usize) -> LaSsize,
        *mut c_void,
    ) -> c_int,
    write_header: unsafe extern "C" fn(LaArchive, LaEntry) -> c_int,
    write_data: unsafe extern "C" fn(LaArchive, *const c_void, usize) -> LaSsize,
    write_finish_entry: unsafe extern "C" fn(LaArchive) -> c_int,
    write_data_block: unsafe extern "C" fn(LaArchive, *const c_void, usize, LaInt64) -> LaSsize,
    error_string: unsafe extern "C" fn(LaArchive) -> *const c_char,
    read_disk_new: unsafe extern "C" fn() -> LaArchive,
    read_disk_set_standard_lookup: unsafe extern "C" fn(LaArchive) -> c_int,
    read_disk_entry_from_file:
        unsafe extern "C" fn(LaArchive, LaEntry, c_int, *const c_void) -> c_int,
    read_new: unsafe extern "C" fn() -> LaArchive,
    read_free: unsafe extern "C" fn(LaArchive) -> c_int,
    read_close: unsafe extern "C" fn(LaArchive) -> c_int,
    read_support_filter_all: unsafe extern "C" fn(LaArchive) -> c_int,
    read_support_format_all: unsafe extern "C" fn(LaArchive) -> c_int,
    read_open_filename: unsafe extern "C" fn(LaArchive, *const c_char, usize) -> c_int,
    read_next_header: unsafe extern "C" fn(LaArchive, *mut LaEntry) -> c_int,
    read_data_block:
        unsafe extern "C" fn(LaArchive, *mut *const c_void, *mut usize, *mut LaInt64) -> c_int,
    write_disk_new: unsafe extern "C" fn() -> LaArchive,
    write_disk_set_options: unsafe extern "C" fn(LaArchive, c_int) -> c_int,
    match_new: unsafe extern "C" fn() -> LaArchive,
    match_free: unsafe extern "C" fn(LaArchive) -> c_int,
    match_include_pattern: unsafe extern "C" fn(LaArchive, *const c_char) -> c_int,
    match_excluded: unsafe extern "C" fn(LaArchive, LaEntry) -> c_int,
    match_path_unmatched_inclusions_next:
        unsafe extern "C" fn(LaArchive, *mut *const c_char) -> c_int,
    entry_new: unsafe extern "C" fn() -> LaEntry,
    entry_free: unsafe extern "C" fn(LaEntry),
    entry_copy_sourcepath: unsafe extern "C" fn(LaEntry, *const c_char),
    entry_copy_pathname: unsafe extern "C" fn(LaEntry, *const c_char),
    entry_set_mtime: unsafe extern "C" fn(LaEntry, i64, c_long),
    entry_set_atime: unsafe extern "C" fn(LaEntry, i64, c_long),
    entry_set_ctime: unsafe extern "C" fn(LaEntry, i64, c_long),
    entry_set_birthtime: unsafe extern "C" fn(LaEntry, i64, c_long),
    entry_acl_clear: unsafe extern "C" fn(LaEntry),
    entry_xattr_clear: unsafe extern "C" fn(LaEntry),
    entry_set_fflags: unsafe extern "C" fn(LaEntry, u64, u64),
    entry_sparse_clear: unsafe extern "C" fn(LaEntry),
    entry_set_uid: unsafe extern "C" fn(LaEntry, LaInt64),
    entry_set_gid: unsafe extern "C" fn(LaEntry, LaInt64),
    entry_set_uname: unsafe extern "C" fn(LaEntry, *const c_char),
    entry_set_gname: unsafe extern "C" fn(LaEntry, *const c_char),
    entry_set_perm: unsafe extern "C" fn(LaEntry, c_int),
    entry_symlink: unsafe extern "C" fn(LaEntry) -> *const c_char,
    entry_hardlink: unsafe extern "C" fn(LaEntry) -> *const c_char,
    entry_size: unsafe extern "C" fn(LaEntry) -> LaInt64,
    entry_pathname: unsafe extern "C" fn(LaEntry) -> *const c_char,
    entry_strmode: unsafe extern "C" fn(LaEntry) -> *const c_char,
    entry_nlink: unsafe extern "C" fn(LaEntry) -> c_int,
    entry_uname: unsafe extern "C" fn(LaEntry) -> *const c_char,
    entry_gname: unsafe extern "C" fn(LaEntry) -> *const c_char,
    entry_uid: unsafe extern "C" fn(LaEntry) -> LaInt64,
    entry_gid: unsafe extern "C" fn(LaEntry) -> LaInt64,
    entry_filetype: unsafe extern "C" fn(LaEntry) -> c_int,
    entry_rdevmajor: unsafe extern "C" fn(LaEntry) -> c_int,
    entry_rdevminor: unsafe extern "C" fn(LaEntry) -> c_int,
    entry_mtime: unsafe extern "C" fn(LaEntry) -> i64,
}

// SAFETY: the table holds plain function pointers into a library that stays
// loaded for the life of the process.
unsafe impl Send for La {}
unsafe impl Sync for La {}

fn la() -> Result<&'static La, String> {
    static LA: OnceLock<Result<La, String>> = OnceLock::new();
    LA.get_or_init(load).as_ref().map_err(Clone::clone)
}

fn load() -> Result<La, String> {
    let mut handle = std::ptr::null_mut();
    let mut reason = String::new();
    for name in [c"libarchive.so.13", c"libarchive.so"] {
        // SAFETY: dlopen with a valid NUL-terminated name.
        handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if !handle.is_null() {
            break;
        }
        // SAFETY: dlerror returns a thread-local message or null.
        let message = unsafe { libc::dlerror() };
        if !message.is_null() && reason.is_empty() {
            // SAFETY: non-null dlerror result is a C string.
            reason = unsafe { CStr::from_ptr(message) }
                .to_string_lossy()
                .into_owned();
        }
    }
    if handle.is_null() {
        return Err(format!("could not load libarchive: {reason}"));
    }
    macro_rules! sym {
        ($name:literal) => {{
            // SAFETY: handle is a loaded library; the name is NUL-terminated.
            let symbol = unsafe { libc::dlsym(handle, concat!($name, "\0").as_ptr().cast()) };
            if symbol.is_null() {
                return Err(format!("libarchive does not export {}", $name));
            }
            // SAFETY: the symbol has the C signature declared for the field.
            #[allow(clippy::missing_transmute_annotations)]
            let function = unsafe { std::mem::transmute::<*mut c_void, _>(symbol) };
            function
        }};
    }
    Ok(La {
        write_new: sym!("archive_write_new"),
        write_free: sym!("archive_write_free"),
        write_add_filter_none: sym!("archive_write_add_filter_none"),
        write_add_filter_compress: sym!("archive_write_add_filter_compress"),
        write_add_filter_gzip: sym!("archive_write_add_filter_gzip"),
        write_add_filter_bzip2: sym!("archive_write_add_filter_bzip2"),
        write_add_filter_lzma: sym!("archive_write_add_filter_lzma"),
        write_add_filter_xz: sym!("archive_write_add_filter_xz"),
        write_add_filter_zstd: sym!("archive_write_add_filter_zstd"),
        write_set_filter_option: sym!("archive_write_set_filter_option"),
        write_set_format_by_name: sym!("archive_write_set_format_by_name"),
        write_set_bytes_in_last_block: sym!("archive_write_set_bytes_in_last_block"),
        write_open: sym!("archive_write_open"),
        write_header: sym!("archive_write_header"),
        write_data: sym!("archive_write_data"),
        write_finish_entry: sym!("archive_write_finish_entry"),
        write_data_block: sym!("archive_write_data_block"),
        error_string: sym!("archive_error_string"),
        read_disk_new: sym!("archive_read_disk_new"),
        read_disk_set_standard_lookup: sym!("archive_read_disk_set_standard_lookup"),
        read_disk_entry_from_file: sym!("archive_read_disk_entry_from_file"),
        read_new: sym!("archive_read_new"),
        read_free: sym!("archive_read_free"),
        read_close: sym!("archive_read_close"),
        read_support_filter_all: sym!("archive_read_support_filter_all"),
        read_support_format_all: sym!("archive_read_support_format_all"),
        read_open_filename: sym!("archive_read_open_filename"),
        read_next_header: sym!("archive_read_next_header"),
        read_data_block: sym!("archive_read_data_block"),
        write_disk_new: sym!("archive_write_disk_new"),
        write_disk_set_options: sym!("archive_write_disk_set_options"),
        match_new: sym!("archive_match_new"),
        match_free: sym!("archive_match_free"),
        match_include_pattern: sym!("archive_match_include_pattern"),
        match_excluded: sym!("archive_match_excluded"),
        match_path_unmatched_inclusions_next: sym!("archive_match_path_unmatched_inclusions_next"),
        entry_new: sym!("archive_entry_new"),
        entry_free: sym!("archive_entry_free"),
        entry_copy_sourcepath: sym!("archive_entry_copy_sourcepath"),
        entry_copy_pathname: sym!("archive_entry_copy_pathname"),
        entry_set_mtime: sym!("archive_entry_set_mtime"),
        entry_set_atime: sym!("archive_entry_set_atime"),
        entry_set_ctime: sym!("archive_entry_set_ctime"),
        entry_set_birthtime: sym!("archive_entry_set_birthtime"),
        entry_acl_clear: sym!("archive_entry_acl_clear"),
        entry_xattr_clear: sym!("archive_entry_xattr_clear"),
        entry_set_fflags: sym!("archive_entry_set_fflags"),
        entry_sparse_clear: sym!("archive_entry_sparse_clear"),
        entry_set_uid: sym!("archive_entry_set_uid"),
        entry_set_gid: sym!("archive_entry_set_gid"),
        entry_set_uname: sym!("archive_entry_set_uname"),
        entry_set_gname: sym!("archive_entry_set_gname"),
        entry_set_perm: sym!("archive_entry_set_perm"),
        entry_symlink: sym!("archive_entry_symlink"),
        entry_hardlink: sym!("archive_entry_hardlink"),
        entry_size: sym!("archive_entry_size"),
        entry_pathname: sym!("archive_entry_pathname"),
        entry_strmode: sym!("archive_entry_strmode"),
        entry_nlink: sym!("archive_entry_nlink"),
        entry_uname: sym!("archive_entry_uname"),
        entry_gname: sym!("archive_entry_gname"),
        entry_uid: sym!("archive_entry_uid"),
        entry_gid: sym!("archive_entry_gid"),
        entry_filetype: sym!("archive_entry_filetype"),
        entry_rdevmajor: sym!("archive_entry_rdevmajor"),
        entry_rdevminor: sym!("archive_entry_rdevminor"),
        entry_mtime: sym!("archive_entry_mtime"),
    })
}

fn cstr(value: *const c_char) -> String {
    if value.is_null() {
        return String::new();
    }
    // SAFETY: libarchive returns NUL-terminated strings.
    unsafe { CStr::from_ptr(value) }
        .to_string_lossy()
        .into_owned()
}

fn error_string(la: &La, archive: LaArchive) -> String {
    // SAFETY: archive is a live libarchive object.
    let text = unsafe { (la.error_string)(archive) };
    if text.is_null() {
        "unknown error".to_owned()
    } else {
        cstr(text)
    }
}

fn cstring(value: &str) -> CString {
    CString::new(value).unwrap_or_else(|_| CString::new(value.replace('\0', "")).unwrap())
}

/// `cmArchiveWrite`: sequential archive writer fed by `archive_read_disk`.
pub struct ArchiveWrite {
    archive: LaArchive,
    disk: LaArchive,
    out: File,
    error: String,
    mtime: String,
    verbose: bool,
    format: String,
    uid: Option<i64>,
    gid: Option<i64>,
    uname: String,
    gname: String,
    permissions: Option<i32>,
}

unsafe extern "C" fn write_cb(
    _archive: LaArchive,
    client_data: *mut c_void,
    buffer: *const c_void,
    length: usize,
) -> LaSsize {
    // SAFETY: client_data is a live ArchiveWrite for the write session;
    // buffer is valid for `length` bytes.
    let writer = unsafe { &mut *(client_data as *mut ArchiveWrite) };
    if writer
        .out
        .write_all(unsafe { std::slice::from_raw_parts(buffer.cast(), length) })
        .is_ok()
    {
        length as LaSsize
    } else {
        -1
    }
}

impl ArchiveWrite {
    /// `cmArchiveWrite::cmArchiveWrite`. `error` is set when construction
    /// failed, mirroring `cmArchiveWrite::Error`.
    pub fn new(
        out: File,
        compress: Compress,
        format: &str,
        compression_level: i32,
        num_threads: i32,
    ) -> ArchiveWrite {
        let la = la().expect("libarchive");
        let num_threads = if num_threads < 1 {
            let upper = if num_threads == 0 {
                i32::MAX
            } else {
                num_threads.abs()
            };
            std::thread::available_parallelism()
                .map(|n| n.get() as i32)
                .unwrap_or(1)
                .clamp(1, upper)
        } else {
            num_threads
        };
        let threads = num_threads.to_string();
        // SAFETY: all pointers are live objects or NUL-terminated C strings.
        unsafe {
            let archive = (la.write_new)();
            let disk = (la.read_disk_new)();
            let mut writer = ArchiveWrite {
                archive,
                disk,
                out,
                error: String::new(),
                mtime: String::new(),
                verbose: false,
                format: format.to_owned(),
                uid: None,
                gid: None,
                uname: String::new(),
                gname: String::new(),
                permissions: None,
            };
            macro_rules! check {
                ($result:expr, $name:literal) => {
                    if ($result) != ARCHIVE_OK {
                        writer.error = format!("{}: {}", $name, error_string(la, archive));
                        return writer;
                    }
                };
            }
            match compress {
                Compress::None => {
                    check!(
                        (la.write_add_filter_none)(archive),
                        "archive_write_add_filter_none"
                    )
                }
                Compress::Compress => {
                    check!(
                        (la.write_add_filter_compress)(archive),
                        "archive_write_add_filter_compress"
                    )
                }
                Compress::GZip => {
                    check!(
                        (la.write_add_filter_gzip)(archive),
                        "archive_write_add_filter_gzip"
                    );
                    if !std::env::var_os("SOURCE_DATE_EPOCH").is_none_or(|value| value.is_empty()) {
                        check!(
                            (la.write_set_filter_option)(
                                archive,
                                c"gzip".as_ptr(),
                                c"timestamp".as_ptr(),
                                std::ptr::null()
                            ),
                            "archive_write_set_filter_option"
                        );
                    }
                }
                Compress::BZip2 => {
                    check!(
                        (la.write_add_filter_bzip2)(archive),
                        "archive_write_add_filter_bzip2"
                    )
                }
                Compress::Lzma => {
                    check!(
                        (la.write_add_filter_lzma)(archive),
                        "archive_write_add_filter_lzma"
                    )
                }
                Compress::Xz => {
                    check!(
                        (la.write_add_filter_xz)(archive),
                        "archive_write_add_filter_xz"
                    );
                    check!(
                        (la.write_set_filter_option)(
                            archive,
                            c"xz".as_ptr(),
                            c"threads".as_ptr(),
                            cstring(&threads).as_ptr()
                        ),
                        "archive_compressor_xz_options"
                    );
                }
                Compress::Zstd => {
                    check!(
                        (la.write_add_filter_zstd)(archive),
                        "archive_write_add_filter_zstd"
                    );
                    check!(
                        (la.write_set_filter_option)(
                            archive,
                            c"zstd".as_ptr(),
                            c"threads".as_ptr(),
                            cstring(&threads).as_ptr()
                        ),
                        "archive_compressor_zstd_options"
                    );
                }
            }
            if compression_level != 0 {
                let filter = match compress {
                    Compress::GZip => Some("gzip"),
                    Compress::BZip2 => Some("bzip2"),
                    Compress::Lzma => Some("lzma"),
                    Compress::Xz => Some("xz"),
                    Compress::Zstd => Some("zstd"),
                    _ => None,
                };
                if let Some(filter) = filter {
                    check!(
                        (la.write_set_filter_option)(
                            archive,
                            cstring(filter).as_ptr(),
                            c"compression-level".as_ptr(),
                            cstring(&compression_level.to_string()).as_ptr()
                        ),
                        "archive_write_set_filter_option"
                    );
                }
            }
            check!(
                (la.read_disk_set_standard_lookup)(disk),
                "archive_read_disk_set_standard_lookup"
            );
            check!(
                (la.write_set_format_by_name)(archive, cstring(format).as_ptr()),
                "archive_write_set_format_by_name"
            );
            check!(
                (la.write_set_bytes_in_last_block)(archive, 1),
                "archive_write_set_bytes_in_last_block"
            );
            writer
        }
    }

    pub fn set_mtime(&mut self, mtime: &str) {
        self.mtime = mtime.to_owned();
    }

    pub fn set_verbose(&mut self, verbose: bool) {
        self.verbose = verbose;
    }

    pub fn error(&self) -> &str {
        &self.error
    }

    /// `cmArchiveWrite::Open`.
    pub fn open(&mut self) -> bool {
        if !self.error.is_empty() {
            return false;
        }
        let la = la().expect("libarchive");
        // SAFETY: self outlives the write session; write_cb reads `out`.
        let result = unsafe {
            (la.write_open)(
                self.archive,
                (self as *mut ArchiveWrite).cast(),
                std::ptr::null_mut(),
                write_cb,
                std::ptr::null_mut(),
            )
        };
        if result != ARCHIVE_OK {
            self.error = format!("archive_write_open: {}", error_string(la, self.archive));
            return false;
        }
        true
    }

    /// `cmArchiveWrite::SetUIDAndGID` / `ClearUIDAndGID`.
    pub fn set_uid_gid(&mut self, uid: i64, gid: i64) {
        self.uid = Some(uid);
        self.gid = Some(gid);
    }

    #[allow(dead_code)]
    pub fn clear_uid_gid(&mut self) {
        self.uid = None;
        self.gid = None;
    }

    /// `cmArchiveWrite::SetUNAMEAndGNAME` / `ClearUNAMEAndGNAME`.
    pub fn set_uname_gname(&mut self, uname: &str, gname: &str) {
        self.uname = uname.to_owned();
        self.gname = gname.to_owned();
    }

    #[allow(dead_code)]
    pub fn clear_uname_gname(&mut self) {
        self.uname.clear();
        self.gname.clear();
    }

    /// `cmArchiveWrite::SetPermissions` / `ClearPermissions`.
    pub fn set_permissions(&mut self, permissions: i32) {
        self.permissions = Some(permissions);
    }

    pub fn clear_permissions(&mut self) {
        self.permissions = None;
    }

    /// `cmArchiveWrite::Add`.
    pub fn add(&mut self, path: &str) -> bool {
        self.add_options(path, 0, None, true)
    }

    /// `cmArchiveWrite::Add(path, skip, prefix, recursive)`.
    pub fn add_options(
        &mut self,
        path: &str,
        skip: usize,
        prefix: Option<&str>,
        recursive: bool,
    ) -> bool {
        let path = path.trim_end_matches('/');
        self.add_path(path, skip, prefix, recursive);
        self.error.is_empty()
    }

    fn add_path(&mut self, path: &str, skip: usize, prefix: Option<&str>, recursive: bool) -> bool {
        if (path != "." || !matches!(self.format.as_str(), "zip" | "7zip"))
            && !self.add_file(path, skip, prefix)
        {
            return false;
        }
        let is_dir = Path::new(path).is_dir();
        let is_symlink = fs::symlink_metadata(path)
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false);
        if !is_dir || !recursive || is_symlink {
            return true;
        }
        if let Ok(read) = fs::read_dir(path) {
            let mut next = format!("{path}/");
            if next == *"./" && matches!(self.format.as_str(), "zip" | "7zip") {
                next.clear();
            }
            let end = next.len();
            for item in read.flatten() {
                let name = item.file_name();
                if name == "." || name == ".." {
                    continue;
                }
                next.truncate(end);
                next.push_str(&name.to_string_lossy());
                if !self.add_path(&next, skip, prefix, true) {
                    return false;
                }
            }
        }
        true
    }

    fn add_file(&mut self, file: &str, skip: usize, prefix: Option<&str>) -> bool {
        self.error.clear();
        if skip >= file.len() {
            return true;
        }
        let la = la().expect("libarchive");
        let out = &file[skip..];
        let dest = format!("{}{}", prefix.unwrap_or_default(), out);
        if self.verbose {
            println!("{dest}");
        }
        // SAFETY: entry/archive objects are live; C strings are NUL-terminated.
        unsafe {
            let entry = (la.entry_new)();
            let guard = EntryGuard { la, entry };
            (la.entry_copy_sourcepath)(entry, cstring(file).as_ptr());
            (la.entry_copy_pathname)(entry, cstring(&dest).as_ptr());
            if (la.read_disk_entry_from_file)(self.disk, entry, -1, std::ptr::null()) != ARCHIVE_OK
            {
                self.error = format!(
                    "Unable to read from file '{file}': {}",
                    error_string(la, self.disk)
                );
                return false;
            }
            if !self.mtime.is_empty() {
                let now = libc::time(std::ptr::null_mut());
                let t = get_date(now, &self.mtime);
                if t == -1 {
                    self.error = format!("unable to parse mtime '{}'", self.mtime);
                    return false;
                }
                (la.entry_set_mtime)(entry, t, 0);
            } else if let Ok(epoch) = std::env::var("SOURCE_DATE_EPOCH")
                && let Ok(time) = epoch.trim().parse::<i64>()
            {
                (la.entry_set_mtime)(entry, time, 0);
                (la.entry_set_atime)(entry, time, 0);
                (la.entry_set_ctime)(entry, time, 0);
                (la.entry_set_birthtime)(entry, time, 0);
            }
            if let (Some(uid), Some(gid)) = (self.uid, self.gid) {
                (la.entry_set_uid)(entry, uid);
                (la.entry_set_gid)(entry, gid);
            }
            if !self.uname.is_empty() && !self.gname.is_empty() {
                (la.entry_set_uname)(entry, cstring(&self.uname).as_ptr());
                (la.entry_set_gname)(entry, cstring(&self.gname).as_ptr());
            }
            if let Some(permissions) = self.permissions {
                (la.entry_set_perm)(entry, permissions);
            }
            (la.entry_acl_clear)(entry);
            (la.entry_xattr_clear)(entry);
            (la.entry_set_fflags)(entry, 0, 0);
            if matches!(self.format.as_str(), "pax" | "paxr") {
                (la.entry_sparse_clear)(entry);
            }
            if (la.write_header)(self.archive, entry) != ARCHIVE_OK {
                self.error = format!("archive_write_header: {}", error_string(la, self.archive));
                return false;
            }
            if (la.entry_symlink)(entry).is_null() {
                let size = (la.entry_size)(entry);
                if size > 0 {
                    drop(guard);
                    return self.add_data(file, size as usize);
                }
            }
        }
        true
    }

    fn add_data(&mut self, file: &str, size: usize) -> bool {
        let la = la().expect("libarchive");
        let mut input = match File::open(file) {
            Ok(input) => input,
            Err(error) => {
                self.error = format!(
                    "Error opening \"{file}\": {}",
                    crate::cmcmd::os_error(&error)
                );
                return false;
            }
        };
        let mut buffer = [0u8; 16384];
        let mut left = size;
        while left > 0 {
            let next = left.min(buffer.len());
            let Ok(read) = input.read(&mut buffer[..next]) else {
                break;
            };
            if read != next {
                break;
            }
            // SAFETY: buffer is valid for `next` bytes.
            if unsafe { (la.write_data)(self.archive, buffer.as_ptr().cast(), next) }
                != next as LaSsize
            {
                self.error = format!("archive_write_data: {}", error_string(la, self.archive));
                return false;
            }
            left -= next;
        }
        if left > 0 {
            self.error = format!("Error reading \"{file}\": unexpected end of file");
            return false;
        }
        true
    }
}

struct EntryGuard<'a> {
    la: &'a La,
    entry: LaEntry,
}

impl Drop for EntryGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: entry was created by archive_entry_new.
        unsafe { (self.la.entry_free)(self.entry) }
    }
}

impl Drop for ArchiveWrite {
    fn drop(&mut self) {
        if let Ok(la) = la() {
            // SAFETY: both objects were created by libarchive constructors.
            unsafe {
                (la.read_free)(self.disk);
                (la.write_free)(self.archive);
            }
        }
    }
}

/// `cmSystemTools::CreateTar` (without the Windows delay).
#[allow(clippy::too_many_arguments)]
pub fn create_tar(
    out_file_name: &str,
    files: &[String],
    working_directory: &str,
    compress: Compress,
    verbose: bool,
    mtime: &str,
    format: &str,
    compression_level: i32,
) -> bool {
    let mut workdir = None;
    if !working_directory.is_empty() {
        workdir = std::env::current_dir().ok();
        if std::env::set_current_dir(working_directory).is_err() {
            crate::cmcmd::error(format!(
                "Failed to change to directory \"{working_directory}\""
            ));
            return false;
        }
    }
    let result = create_tar_inner(
        out_file_name,
        files,
        compress,
        verbose,
        mtime,
        format,
        compression_level,
    );
    if let Some(previous) = workdir {
        let _ = std::env::set_current_dir(previous);
    }
    result
}

fn create_tar_inner(
    out_file_name: &str,
    files: &[String],
    compress: Compress,
    verbose: bool,
    mtime: &str,
    format: &str,
    compression_level: i32,
) -> bool {
    if let Err(reason) = la() {
        crate::cmcmd::error(reason);
        return false;
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let out = match File::create(out_file_name) {
        Ok(out) => out,
        Err(error) => {
            crate::cmcmd::error(format!(
                "Cannot open output file \"{out_file_name}\": {}",
                crate::cmcmd::os_error(&error)
            ));
            return false;
        }
    };
    let mut writer = ArchiveWrite::new(
        out,
        compress,
        if format.is_empty() { "paxr" } else { format },
        compression_level,
        0,
    );
    if !writer.open() {
        crate::cmcmd::error(writer.error().to_owned());
        return false;
    }
    writer.set_mtime(mtime);
    writer.set_verbose(verbose);
    let mut success = true;
    for path in files {
        let path = if Path::new(path).is_absolute() {
            to_relative(&cwd.to_string_lossy(), path)
        } else {
            path.clone()
        };
        if !writer.add(&path) {
            crate::cmcmd::error(writer.error().to_owned());
            success = false;
        }
    }
    success
}

/// `cmSystemTools::ConvertToRelativePath`: `remote` expressed relative to
/// the `local` directory (both already absolute).
fn to_relative(local: &str, remote: &str) -> String {
    let local: Vec<&str> = local.split('/').filter(|part| !part.is_empty()).collect();
    let remote: Vec<&str> = remote.split('/').filter(|part| !part.is_empty()).collect();
    let common = local
        .iter()
        .zip(&remote)
        .take_while(|(left, right)| left == right)
        .count();
    let mut relative = String::new();
    for _ in &local[common..] {
        relative.push_str("../");
    }
    relative.push_str(&remote[common..].join("/"));
    if relative.is_empty() {
        ".".to_owned()
    } else {
        relative
    }
}

/// `cmSystemTools::ExtractTar`.
pub fn extract_tar(
    out_file_name: &str,
    files: &[String],
    timestamps: ExtractTimestamps,
    verbose: bool,
) -> bool {
    extract_tar_inner(out_file_name, files, verbose, timestamps, true)
}

/// `cmSystemTools::ListTar`.
pub fn list_tar(out_file_name: &str, files: &[String], verbose: bool) -> bool {
    extract_tar_inner(out_file_name, files, verbose, ExtractTimestamps::Yes, false)
}

fn archive_error(prefix: &str, archive: LaArchive) {
    let la = la().expect("libarchive");
    let mut message = prefix.to_owned();
    // SAFETY: archive is a live object.
    let text = unsafe { (la.error_string)(archive) };
    if !text.is_null() {
        message.push_str(&cstr(text));
    }
    crate::cmcmd::error(message);
}

fn la_diagnostic(archive: LaArchive, result: LaSsize) -> bool {
    let la = la().expect("libarchive");
    if result >= i64::from(ARCHIVE_OK) {
        return true;
    }
    // SAFETY: archive is a live object.
    let text = unsafe { (la.error_string)(archive) };
    if result >= i64::from(ARCHIVE_WARN) {
        eprintln!(
            "cmake -E tar: warning: {}",
            if text.is_null() {
                "unknown warning".to_owned()
            } else {
                cstr(text)
            }
        );
        return true;
    }
    eprintln!(
        "cmake -E tar: error: {}",
        if text.is_null() {
            "unknown error".to_owned()
        } else {
            cstr(text)
        }
    );
    false
}

fn entry_pathname(la: &La, entry: LaEntry) -> String {
    // SAFETY: entry is a live object.
    unsafe { cstr((la.entry_pathname)(entry)) }
}

fn copy_data(ar: LaArchive, aw: LaArchive) -> bool {
    let la = la().expect("libarchive");
    loop {
        let mut buffer = std::ptr::null();
        let mut size = 0usize;
        let mut offset = 0i64;
        // SAFETY: archives are live; out params are valid.
        let result = unsafe { (la.read_data_block)(ar, &mut buffer, &mut size, &mut offset) };
        if result == ARCHIVE_EOF {
            return true;
        }
        if !la_diagnostic(ar, i64::from(result)) {
            return false;
        }
        // SAFETY: buffer points to `size` readable bytes.
        let written = unsafe { (la.write_data_block)(aw, buffer, size, offset) };
        if !la_diagnostic(ar, written) {
            return false;
        }
    }
}

fn extract_tar_inner(
    out_file_name: &str,
    files: &[String],
    verbose: bool,
    timestamps: ExtractTimestamps,
    extract: bool,
) -> bool {
    let la = match la() {
        Ok(la) => la,
        Err(reason) => {
            crate::cmcmd::error(reason);
            return false;
        }
    };
    // SAFETY: all objects created here are freed before return; entries
    // returned by archive_read_next_header are owned by the archive.
    unsafe {
        let a = (la.read_new)();
        let ext = (la.write_disk_new)();
        (la.read_support_filter_all)(a);
        (la.read_support_format_all)(a);
        let matching = (la.match_new)();
        if matching.is_null() {
            crate::cmcmd::error("Out of memory");
            return false;
        }
        for filename in files {
            if (la.match_include_pattern)(matching, cstring(filename).as_ptr()) != ARCHIVE_OK {
                crate::cmcmd::error(format!("Failed to add to inclusion list: {filename}"));
                return false;
            }
        }
        let result = if out_file_name == "-" {
            (la.read_open_filename)(a, std::ptr::null(), 10240)
        } else {
            (la.read_open_filename)(a, cstring(out_file_name).as_ptr(), 10240)
        };
        if result != 0 {
            archive_error("Problem with archive_read_open_file(): ", a);
            (la.write_free)(ext);
            (la.read_close)(a);
            return false;
        }
        let mut r;
        loop {
            let mut entry = std::ptr::null_mut();
            r = (la.read_next_header)(a, &mut entry);
            if r == ARCHIVE_EOF {
                break;
            }
            if r != ARCHIVE_OK {
                archive_error("Problem with archive_read_next_header(): ", a);
                break;
            }
            if (la.match_excluded)(matching, entry) != 0 {
                continue;
            }
            if verbose {
                if extract {
                    println!("x {}", entry_pathname(la, entry));
                } else {
                    list_item_verbose(la, entry);
                    println!();
                }
            } else if !extract {
                println!("{}", entry_pathname(la, entry));
            }
            if extract {
                if timestamps == ExtractTimestamps::Yes {
                    r = (la.write_disk_set_options)(ext, ARCHIVE_EXTRACT_TIME);
                    if r != ARCHIVE_OK {
                        archive_error("Problem with archive_write_disk_set_options(): ", ext);
                        break;
                    }
                }
                r = (la.write_header)(ext, entry);
                if r == ARCHIVE_OK {
                    if !copy_data(a, ext) {
                        r = -1;
                        break;
                    }
                    r = (la.write_finish_entry)(ext);
                    if r != ARCHIVE_OK {
                        archive_error("Problem with archive_write_finish_entry(): ", ext);
                        break;
                    }
                } else {
                    archive_error("Problem with archive_write_header(): ", ext);
                    crate::cmcmd::error(format!("Current file: {}", entry_pathname(la, entry)));
                    break;
                }
            }
        }

        let mut error_occurred = false;
        let mut pattern = std::ptr::null();
        while (la.match_path_unmatched_inclusions_next)(matching, &mut pattern) == ARCHIVE_OK {
            crate::cmcmd::error(format!("tar: {}: Not found in archive", cstr(pattern)));
            error_occurred = true;
        }
        if error_occurred {
            return false;
        }
        (la.match_free)(matching);
        (la.write_free)(ext);
        (la.read_close)(a);
        (la.read_free)(a);
        r == ARCHIVE_EOF || r == ARCHIVE_OK
    }
}

/// The `list_item_verbose` helper of cmSystemTools.cxx.
fn list_item_verbose(la: &La, entry: LaEntry) {
    // SAFETY: entry is a live libarchive object.
    unsafe {
        let strmode = cstr((la.entry_strmode)(entry));
        let nlink = (la.entry_nlink)(entry);
        let mut p = (la.entry_uname)(entry);
        let mut user = cstr(p);
        if user.is_empty() {
            user = format!("{} ", (la.entry_uid)(entry));
        }
        let u_width = user.len().max(6);
        let group_raw = (la.entry_gname)(entry);
        let (group, w) = if group_raw.is_null() || *group_raw == 0 {
            let id = (la.entry_gid)(entry).to_string();
            let w = id.len();
            (id, w)
        } else {
            let name = cstr(group_raw);
            let w = name.len();
            (name, w)
        };
        let size = if matches!((la.entry_filetype)(entry), AE_IFCHR | AE_IFBLK) {
            format!(
                "{},{}",
                (la.entry_rdevmajor)(entry),
                (la.entry_rdevminor)(entry)
            )
        } else {
            (la.entry_size)(entry).to_string()
        };
        let gs_width = (w + size.len() + 1).max(13);
        let mtime = (la.entry_mtime)(entry) as libc::time_t;
        let date = format_date(mtime);
        print!(
            "{strmode} {nlink} {user:u_width$} {group}{size:>width$} {date} {}",
            entry_pathname(la, entry),
            width = gs_width - w,
            u_width = u_width
        );
        p = (la.entry_hardlink)(entry);
        if !p.is_null() {
            print!(" link to {}", cstr(p));
        } else {
            p = (la.entry_symlink)(entry);
            if !p.is_null() {
                print!(" -> {}", cstr(p));
            }
        }
    }
}

fn format_date(timestamp: libc::time_t) -> String {
    // SAFETY: libc writes a tm and a NUL-terminated result within the buffer.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&timestamp, &mut tm).is_null() {
            return String::new();
        }
        let format = if timestamp.abs_diff(now) > 365 * 86400 / 2 {
            c"%e %b  %Y"
        } else {
            c"%e %b %H:%M"
        };
        let mut buffer = [0; 64];
        libc::strftime(buffer.as_mut_ptr(), buffer.len(), format.as_ptr(), &tm);
        CStr::from_ptr(buffer.as_ptr())
            .to_string_lossy()
            .into_owned()
    }
}

/// The subset of `cm_get_date` formats exercised by `--mtime=`. Supports
/// `@epoch`, `epoch`, `YYYY-MM-DD HH:MM:SS UTC`, RFC-3339, `YYYY-MM-DDUTC`
/// and `YYYY-MM-DD`, like the Rust tar frontend did before this module.
pub fn get_date(_now: i64, value: &str) -> i64 {
    if let Ok(timestamp) = value.strip_prefix('@').unwrap_or(value).parse::<i64>() {
        return timestamp;
    }
    let Ok(value) = CString::new(value) else {
        return -1;
    };
    for format in [
        c"%Y-%m-%d %H:%M:%S UTC",
        c"%Y-%m-%dT%H:%M:%SZ",
        c"%Y-%m-%dUTC",
        c"%Y-%m-%d",
    ] {
        // SAFETY: All pointers are valid for strptime; zero is a valid tm.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        let end = unsafe { libc::strptime(value.as_ptr(), format.as_ptr(), &mut tm) };
        if !end.is_null() && unsafe { *end == 0 } {
            return unsafe { libc::timegm(&mut tm) };
        }
    }
    -1
}
