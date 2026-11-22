//! cmcmd `tar` option parsing (cmcmd.cxx) on top of `carchive`, the
//! libarchive-backed port of `cmSystemTools::CreateTar`/`ExtractTar`/`ListTar`.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use super::error;
use crate::carchive::{self, Compress, ExtractTimestamps};

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
        't' => (
            "listing",
            carchive::list_tar(path, &options.files, options.verbose),
        ),
        'x' => (
            "extracting",
            carchive::extract_tar(
                path,
                &options.files,
                if options.touch {
                    ExtractTimestamps::No
                } else {
                    ExtractTimestamps::Yes
                },
                options.verbose,
            ),
        ),
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
    if opts.files.is_empty() {
        eprintln!("tar: No files or directories specified");
    }
    let compression = match opts.compression {
        'j' => Compress::BZip2,
        'J' => Compress::Xz,
        'Z' => Compress::Zstd,
        'z' => Compress::GZip,
        _ => Compress::None,
    };
    carchive::create_tar(
        path,
        &opts.files,
        "",
        compression,
        opts.verbose,
        opts.mtime,
        opts.format,
        0,
    )
}
