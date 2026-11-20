//! cmcmd tar parsing and the portable tar/gzip formats.

use std::ffi::{CStr, CString};
use std::fs::{self, File, Metadata};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
use std::path::Path;

use flate2::{Compression, read::MultiGzDecoder, write::GzEncoder};
use tar::{Archive, Builder, EntryType, Header};

use super::{error, os_error};

#[derive(Default)]
struct Options<'a> {
    action: char,
    compression: char,
    compression_count: usize,
    format: &'a str,
    mtime: &'a str,
    verbose: bool,
    touch: bool,
    files: Vec<String>,
}

pub(super) fn tar(args: &[String]) -> i32 {
    let path = &args[1];
    let Some(options) = parse_options(args) else {
        return 1;
    };
    let (action, result) = match options.action {
        'c' => ("creating", create(path, &options)),
        't' => ("listing", read(path, &options, false)),
        'x' => ("extracting", read(path, &options, true)),
        _ => {
            error(
                "tar: No action specified. Please choose: 't' (list), 'c' (create) or 'x' (extract)",
            );
            return 1;
        }
    };
    if !result {
        error(format!("Problem {action} tar: {path}"));
    }
    i32::from(!result)
}

fn parse_options(args: &[String]) -> Option<Options<'_>> {
    let mut options = Options::default();
    let mut flags = true;
    for arg in &args[2..] {
        if !flags || !arg.starts_with("--") {
            options.files.push(arg.clone());
        } else if arg == "--" {
            flags = false;
        } else if arg == "--zstd" {
            options.compression = 'Z';
            options.compression_count += 1;
        } else if arg == "--touch" {
            options.touch = true;
        } else if let Some(value) = arg.strip_prefix("--mtime=") {
            options.mtime = value;
        } else if let Some(value) = arg.strip_prefix("--format=") {
            options.format = value;
            if !matches!(value, "7zip" | "gnutar" | "pax" | "paxr" | "zip") {
                error(format!("Unknown -E tar --format= argument: {value}"));
                return None;
            }
        } else if let Some(value) = arg.strip_prefix("--files-from=") {
            files_from(value, &mut options.files)?;
        } else {
            error(format!("Unknown option to -E tar: {arg}"));
            return None;
        }
    }
    for flag in args[0].chars() {
        match flag {
            '-' | 'f' => {}
            'z' | 'j' | 'J' => {
                options.compression = flag;
                options.compression_count += 1;
            }
            'v' => options.verbose = true,
            'c' | 'x' | 't' => options.action = flag,
            _ => eprintln!("tar: Unknown argument: {flag}"),
        }
    }
    if matches!(options.format, "zip" | "7zip") && options.compression_count > 0 {
        error(format!(
            "Can not use compression flags with format: {}",
            options.format
        ));
        return None;
    }
    if options.compression_count > 1 {
        error("Can only compress a tar file one way; at most one flag of z, j, or J may be used");
        return None;
    }
    Some(options)
}

fn files_from(path: &str, files: &mut Vec<String>) -> Option<()> {
    if Path::new(path).is_dir() {
        error(format!("-E tar --files-from= file '{path}' is a directory"));
        return None;
    }
    let Ok(file) = File::open(path) else {
        error(format!("-E tar --files-from= file '{path}' not found"));
        return None;
    };
    for line in BufReader::new(file).lines() {
        let line = line.ok()?;
        if let Some(file) = line.strip_prefix("--add-file=") {
            files.push(file.to_owned());
        } else if line.starts_with('-') {
            error(format!(
                "-E tar --files-from='{path}' file invalid line:\n{line}\n"
            ));
            return None;
        } else if !line.is_empty() {
            files.push(line);
        }
    }
    Some(())
}

fn create(path: &str, opts: &Options<'_>) -> bool {
    if matches!(opts.format, "zip" | "7zip") {
        error(format!("tar: format '{}' is not supported", opts.format));
        return false;
    }
    if !matches!(opts.compression, '\0' | 'z') {
        error("tar: only uncompressed and gzip-compressed archives are supported");
        return false;
    }
    if opts.files.is_empty() {
        eprintln!("tar: No files or directories specified");
    }
    let file = match File::create(path) {
        Ok(file) => file,
        Err(err) => {
            error(format!(
                "Cannot open output file \"{path}\": {}",
                os_error(&err)
            ));
            return false;
        }
    };
    if opts.compression == 'z' {
        let mut writer = GzEncoder::new(file, Compression::default());
        let success = write_archive(&mut writer, opts);
        writer.finish().is_ok() && success
    } else {
        write_archive(file, opts)
    }
}

fn write_archive<W: Write>(writer: W, opts: &Options<'_>) -> bool {
    let mut builder = Builder::new(writer);
    let cwd = std::env::current_dir().unwrap_or_default();
    let mut success = true;
    for source in &opts.files {
        let source_path = Path::new(source);
        let name = if source_path.is_absolute() {
            relative_path(&cwd, source_path)
        } else {
            source.clone()
        };
        let name = name.trim_end_matches('/');
        if let Err(message) = append(&mut builder, source_path, name, opts) {
            error(message);
            success = false;
        }
    }
    builder.finish().is_ok() && success
}

pub(super) fn relative_path(base: &Path, path: &Path) -> String {
    let base: Vec<_> = base.components().collect();
    let path: Vec<_> = path.components().collect();
    let common = base
        .iter()
        .zip(&path)
        .take_while(|(left, right)| left == right)
        .count();
    let mut relative = std::path::PathBuf::new();
    for _ in &base[common..] {
        relative.push("..");
    }
    for component in &path[common..] {
        relative.push(component);
    }
    if relative.as_os_str().is_empty() {
        ".".to_owned()
    } else {
        relative.to_string_lossy().into_owned()
    }
}

fn append<W: Write>(
    builder: &mut Builder<W>,
    path: &Path,
    name: &str,
    opts: &Options<'_>,
) -> Result<(), String> {
    if opts.verbose {
        println!("{name}");
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| format!("Unable to read from file '{name}': Can't lstat {name}"))?;
    let mut header = if opts.format == "gnutar" {
        Header::new_gnu()
    } else {
        Header::new_ustar()
    };
    header.set_metadata(&metadata);
    let stored_name = if metadata.is_dir() {
        format!("{name}/")
    } else {
        name.to_owned()
    };
    if let Some(mtime) = (!opts.mtime.is_empty()).then(|| parse_mtime(opts.mtime)) {
        header.set_mtime(mtime.ok_or_else(|| format!("unable to parse mtime '{}'", opts.mtime))?);
    }
    set_names(&mut header, &metadata);
    let entry_error = |err: io::Error| format!("Unable to add file '{name}': {}", os_error(&err));
    if stored_name.len() <= 100 {
        header.as_mut_bytes()[..stored_name.len()].copy_from_slice(stored_name.as_bytes());
    } else {
        write_extended_name(builder, stored_name.as_bytes(), EntryType::GNULongName)
            .map_err(entry_error)?;
        header.as_mut_bytes()[..100].copy_from_slice(&stored_name.as_bytes()[..100]);
    }
    if metadata.is_symlink() {
        let link = fs::read_link(path).map_err(entry_error)?;
        let bytes = link.as_os_str().as_bytes();
        if header.set_link_name_literal(bytes).is_err() {
            write_extended_name(builder, bytes, EntryType::GNULongLink).map_err(entry_error)?;
        }
    }
    header.set_cksum();
    if metadata.is_file() {
        builder
            .append(
                &header,
                File::open(path).map_err(entry_error)?.take(metadata.len()),
            )
            .map_err(entry_error)?;
    } else {
        builder.append(&header, io::empty()).map_err(entry_error)?;
    }
    if metadata.is_dir() {
        for item in fs::read_dir(path).map_err(entry_error)? {
            let item = item.map_err(entry_error)?;
            let child = format!("{name}/{}", item.file_name().to_string_lossy());
            append(builder, &item.path(), &child, opts)?;
        }
    }
    Ok(())
}

fn write_extended_name<W: Write>(
    builder: &mut Builder<W>,
    name: &[u8],
    kind: EntryType,
) -> io::Result<()> {
    let mut header = Header::new_gnu();
    header.set_path("././@LongLink")?;
    header.set_entry_type(kind);
    header.set_size((name.len() + 1) as u64);
    header.set_cksum();
    builder.append(&header, name.chain(&[0][..]))
}

fn parse_mtime(value: &str) -> Option<u64> {
    if let Ok(timestamp) = value.strip_prefix('@').unwrap_or(value).parse() {
        return Some(timestamp);
    }
    let value = CString::new(value).ok()?;
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
            return unsafe { libc::timegm(&mut tm) }.try_into().ok();
        }
    }
    None
}

fn set_names(header: &mut Header, metadata: &Metadata) {
    // SAFETY: The utility runs on one thread and copies each libc result
    // before the corresponding lookup function can be called again.
    unsafe {
        let passwd = libc::getpwuid(metadata.uid());
        if let Some(user) = passwd.as_ref() {
            let _ = header.set_username(&CStr::from_ptr(user.pw_name).to_string_lossy());
        }
        let group = libc::getgrgid(metadata.gid());
        if let Some(group) = group.as_ref() {
            let _ = header.set_groupname(&CStr::from_ptr(group.gr_name).to_string_lossy());
        }
    }
}

fn read(path: &str, opts: &Options<'_>, extract: bool) -> bool {
    let file = if path == "-" {
        Ok(Box::new(io::stdin()) as Box<dyn Read>)
    } else {
        File::open(path).map(|file| Box::new(file) as Box<dyn Read>)
    };
    let Ok(file) = file else {
        error(format!(
            "Problem with archive_read_open_file(): Failed to open '{path}'"
        ));
        return false;
    };
    let mut input = BufReader::new(file);
    let Ok(prefix) = input.fill_buf() else {
        error("Problem with archive_read_open_file(): Unrecognized archive format");
        return false;
    };
    if prefix.starts_with(&[0x1f, 0x8b]) {
        read_archive(MultiGzDecoder::new(input), opts, extract)
    } else {
        if prefix.starts_with(b"PK")
            || prefix.starts_with(b"BZh")
            || prefix.starts_with(b"7z")
            || prefix.starts_with(&[0xfd, b'7', b'z', b'X', b'Z'])
            || prefix.starts_with(&[0x28, 0xb5, 0x2f, 0xfd])
        {
            error("tar: only uncompressed and gzip-compressed tar archives are supported");
            return false;
        }
        read_archive(input, opts, extract)
    }
}

fn read_archive<R: Read>(reader: R, opts: &Options<'_>, extract: bool) -> bool {
    let mut archive = Archive::new(reader);
    archive.set_preserve_mtime(!opts.touch);
    let Ok(entries) = archive.entries() else {
        error("Problem with archive_read_open_file(): Unrecognized archive format");
        return false;
    };
    let mut matched = vec![false; opts.files.len()];
    let mut success = true;
    let mut directories = Vec::new();
    for item in entries {
        let Ok(mut entry) = item else {
            error("Problem with archive_read_next_header(): Unrecognized archive format");
            success = false;
            break;
        };
        let name = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        if !archive_match(&opts.files, &mut matched, &name) {
            continue;
        }
        if extract {
            if opts.verbose {
                println!("x {name}");
            }
            if entry.header().entry_type().is_dir() && matches!(name.as_str(), "." | "./") {
                continue;
            }
            if entry.header().entry_type().is_dir() {
                directories.push(entry);
                continue;
            }
            if !unpack(&mut entry, &name) {
                success = false;
                break;
            }
        } else if opts.verbose {
            verbose_list(&entry, &name);
        } else {
            println!("{name}");
        }
    }
    for mut dir in directories.into_iter().rev() {
        let name = String::from_utf8_lossy(&dir.path_bytes()).into_owned();
        success &= unpack(&mut dir, &name);
    }
    for (path, matched) in opts.files.iter().zip(matched) {
        if !matched {
            error(format!("tar: {path}: Not found in archive"));
            success = false;
        }
    }
    success
}

fn unpack<R: Read>(entry: &mut tar::Entry<'_, R>, name: &str) -> bool {
    match entry.unpack_in(".") {
        Ok(true) => true,
        other => {
            let explanation = match other {
                Err(err) => os_error(&err),
                _ => "Path is outside the extraction directory".to_owned(),
            };
            error(format!(
                "Problem with archive_write_header(): {explanation}"
            ));
            error(format!("Current file: {name}"));
            false
        }
    }
}

fn archive_match(patterns: &[String], matched: &mut [bool], name: &str) -> bool {
    let name = name
        .strip_prefix("./")
        .unwrap_or(name)
        .trim_end_matches('/');
    let mut include = patterns.is_empty();
    for (pattern, matched) in patterns.iter().zip(matched) {
        let pattern = pattern
            .strip_prefix("./")
            .unwrap_or(pattern)
            .trim_end_matches('/');
        if name == pattern
            || name
                .strip_prefix(pattern)
                .is_some_and(|tail| tail.starts_with('/'))
            || glob_match(pattern, name)
        {
            *matched = true;
            include = true;
        }
    }
    include
}

fn glob_match(pattern: &str, name: &str) -> bool {
    let (Ok(pattern), Ok(name)) = (CString::new(pattern), CString::new(name)) else {
        return false;
    };
    // SAFETY: Both arguments are NUL-terminated strings.
    unsafe { libc::fnmatch(pattern.as_ptr(), name.as_ptr(), 0) == 0 }
}

fn verbose_list<R: Read>(entry: &tar::Entry<'_, R>, name: &str) {
    let header = entry.header();
    let kind = header.entry_type();
    let mode = mode_string(kind, header.mode().unwrap_or(0));
    let user = header
        .username()
        .ok()
        .flatten()
        .filter(|name| !name.is_empty())
        .map_or_else(|| format!("{} ", header.uid().unwrap_or(0)), str::to_owned);
    let group = header
        .groupname()
        .ok()
        .flatten()
        .filter(|name| !name.is_empty())
        .map_or_else(|| header.gid().unwrap_or(0).to_string(), str::to_owned);
    let size = entry.size().to_string();
    let width = 13usize.saturating_sub(group.len()).max(size.len() + 1);
    let date = format_date(header.mtime().unwrap_or_default() as libc::time_t);
    print!("{mode}  0 {user:6} {group}{size:>width$} {date} {name}");
    if let Some(link) = entry.link_name_bytes() {
        let separator = if kind.is_hard_link() {
            " link to "
        } else {
            " -> "
        };
        print!("{separator}{}", String::from_utf8_lossy(&link));
    }
    println!();
}

fn mode_string(kind: EntryType, mode: u32) -> String {
    let mut value = match kind {
        EntryType::Directory => "d",
        EntryType::Symlink => "l",
        EntryType::Fifo => "p",
        EntryType::Char => "c",
        EntryType::Block => "b",
        EntryType::Link => "h",
        _ => "-",
    }
    .to_owned();
    for (index, symbol) in "rwxrwxrwx".chars().enumerate() {
        let enabled = mode & (1 << (8 - index)) != 0;
        let symbol = match index {
            2 if mode & 0o4000 != 0 => {
                if enabled {
                    's'
                } else {
                    'S'
                }
            }
            5 if mode & 0o2000 != 0 => {
                if enabled {
                    's'
                } else {
                    'S'
                }
            }
            8 if mode & 0o1000 != 0 => {
                if enabled {
                    't'
                } else {
                    'T'
                }
            }
            _ => {
                if enabled {
                    symbol
                } else {
                    '-'
                }
            }
        };
        value.push(symbol);
    }
    value
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
