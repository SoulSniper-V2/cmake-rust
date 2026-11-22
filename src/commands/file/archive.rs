//! `cmFileCommand`'s `HandleArchiveCreateCommand` and
//! `HandleArchiveExtractCommand` on top of `carchive`.

use crate::carchive::{self, Compress, ExtractTimestamps};
use crate::commands::legacy_fs::{fatal, mkdir};
use crate::interp::{CmdResult, Interp};
use crate::messages::MessageType;
use crate::util::is_full_path;

/// Keyword arity for the ported parsers: `cmArgumentParser` binds
/// single-value (`std::string`), multi-value (`MaybeEmpty<std::vector>`),
/// optional single-value (`Maybe<std::string>`), and flag (`bool`)
/// keywords.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Single,
    Multi,
    Maybe,
    Flag,
}

pub(super) struct Parsed {
    pub(super) singles: Vec<(String, String)>,
    pub(super) multis: Vec<(String, Vec<String>)>,
    pub(super) maybes: Vec<(String, String)>,
    pub(super) flags: Vec<String>,
    pub(super) unrecognized: Vec<String>,
    pub(super) keyword_errors: Vec<(String, String)>,
}

impl Parsed {
    pub(super) fn single(&self, key: &str) -> Option<&str> {
        self.singles
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    pub(super) fn multi(&self, key: &str) -> &[String] {
        self.multis
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_slice())
            .unwrap_or(&[])
    }

    pub(super) fn maybe(&self, key: &str) -> Option<&str> {
        self.maybes
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    pub(super) fn flag(&self, key: &str) -> bool {
        self.flags.iter().any(|name| name == key)
    }
}

pub(super) fn parse(args: &[String], keywords: &[(&'static str, Kind)]) -> Parsed {
    let mut parsed = Parsed {
        singles: Vec::new(),
        multis: Vec::new(),
        maybes: Vec::new(),
        flags: Vec::new(),
        unrecognized: Vec::new(),
        keyword_errors: Vec::new(),
    };
    let missing = |key: &str| {
        (
            key.to_owned(),
            format!("Error after keyword \"{key}\":\n  missing required value\n"),
        )
    };
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        let Some(&(key, kind)) = keywords.iter().find(|(name, _)| name == arg) else {
            parsed.unrecognized.push(arg.clone());
            index += 1;
            continue;
        };
        index += 1;
        match kind {
            Kind::Flag => parsed.flags.push(key.to_owned()),
            Kind::Single => {
                if index < args.len() && !keywords.iter().any(|(n, _)| n == &args[index]) {
                    parsed.singles.push((key.to_owned(), args[index].clone()));
                    index += 1;
                } else {
                    parsed.keyword_errors.push(missing(key));
                }
            }
            Kind::Maybe => {
                let mut value = String::new();
                if index < args.len() && !keywords.iter().any(|(n, _)| n == &args[index]) {
                    value = args[index].clone();
                    index += 1;
                }
                parsed.maybes.push((key.to_owned(), value));
            }
            Kind::Multi => {
                let mut values = Vec::new();
                while index < args.len() && !keywords.iter().any(|(n, _)| n == &args[index]) {
                    values.push(args[index].clone());
                    index += 1;
                }
                parsed.multis.push((key.to_owned(), values));
            }
        }
    }
    // cmArgumentParser stores keyword errors keyed by name; reporting is
    // alphabetical.
    parsed.keyword_errors.sort_by(|(a, _), (b, _)| a.cmp(b));
    parsed
}

/// `ParseResult::MaybeReportError`: issue FATAL_ERROR and report handled.
pub(super) fn maybe_report_error(interp: &mut Interp, parsed: &Parsed) -> bool {
    if parsed.keyword_errors.is_empty() {
        return false;
    }
    let error = parsed
        .keyword_errors
        .iter()
        .map(|(_, text)| text.as_str())
        .collect::<String>();
    interp.issue(MessageType::FatalError, &error);
    interp.set_fatal();
    true
}

pub(super) fn archive_create(interp: &mut Interp, args: &[String]) -> CmdResult {
    let parsed = parse(
        &args[1..],
        &[
            ("OUTPUT", Kind::Single),
            ("FORMAT", Kind::Single),
            ("COMPRESSION", Kind::Single),
            ("COMPRESSION_LEVEL", Kind::Single),
            ("MTIME", Kind::Maybe),
            ("WORKING_DIRECTORY", Kind::Single),
            ("VERBOSE", Kind::Flag),
            ("PATHS", Kind::Multi),
        ],
    );
    if let Some(bad) = parsed.unrecognized.first() {
        return Err(fatal(
            interp,
            &format!("file Unrecognized argument: \"{bad}\""),
        ));
    }
    if maybe_report_error(interp, &parsed) {
        return Ok(());
    }
    let output = parsed.single("OUTPUT").unwrap_or_default();
    let format = parsed.single("FORMAT").unwrap_or_default();
    let compression = parsed.single("COMPRESSION").unwrap_or_default();
    let compression_level = parsed.single("COMPRESSION_LEVEL").unwrap_or_default();
    let mtime = parsed.maybe("MTIME").unwrap_or_default();
    let working_directory = parsed.single("WORKING_DIRECTORY").unwrap_or_default();
    let verbose = parsed.flag("VERBOSE");
    let paths = parsed.multi("PATHS");

    if !format.is_empty() && !matches!(format, "7zip" | "gnutar" | "pax" | "paxr" | "raw" | "zip") {
        return Err(fatal(
            interp,
            &format!("file archive format {format} not supported"),
        ));
    }
    if !compression.is_empty() && matches!(format, "7zip" | "zip") {
        return Err(fatal(
            interp,
            &format!("file archive format {format} does not support COMPRESSION arguments"),
        ));
    }
    let compress = match compression {
        "" | "None" => Compress::None,
        "BZip2" => Compress::BZip2,
        "GZip" => Compress::GZip,
        "XZ" => Compress::Xz,
        "Zstd" => Compress::Zstd,
        other => {
            return Err(fatal(
                interp,
                &format!("file compression type {other} is not supported"),
            ));
        }
    };
    let mut level = 0;
    let max_level = if compress == Compress::Zstd { 19 } else { 9 };
    if !compression_level.is_empty() {
        if compression_level.len() != 1
            && !compression_level
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_digit)
        {
            return Err(fatal(
                interp,
                &format!(
                    "file compression level {compression_level} for {compression} should be in range 0 to {max_level}"
                ),
            ));
        }
        level = compression_level.parse::<i32>().unwrap_or(i32::MAX);
        if !(0..=max_level).contains(&level) {
            return Err(fatal(
                interp,
                &format!(
                    "file compression level {compression_level} for {compression} should be in range 0 to {max_level}"
                ),
            ));
        }
        if compress == Compress::None {
            return Err(fatal(
                interp,
                &format!(
                    "file compression level is not supported for compression \"None\"{compression}"
                ),
            ));
        }
    }
    if paths.is_empty() {
        return Err(fatal(
            interp,
            "file ARCHIVE_CREATE requires a non-empty list of PATHS",
        ));
    }
    if !carchive::create_tar(
        output,
        paths,
        working_directory,
        compress,
        verbose,
        mtime,
        format,
        level,
    ) {
        return Err(fatal(interp, &format!("file failed to compress: {output}")));
    }
    Ok(())
}

pub(super) fn archive_extract(interp: &mut Interp, args: &[String]) -> CmdResult {
    let parsed = parse(
        &args[1..],
        &[
            ("INPUT", Kind::Single),
            ("VERBOSE", Kind::Flag),
            ("LIST_ONLY", Kind::Flag),
            ("DESTINATION", Kind::Single),
            ("PATTERNS", Kind::Multi),
            ("TOUCH", Kind::Flag),
        ],
    );
    if let Some(bad) = parsed.unrecognized.first() {
        return Err(fatal(
            interp,
            &format!("file Unrecognized argument: \"{bad}\""),
        ));
    }
    if maybe_report_error(interp, &parsed) {
        return Ok(());
    }
    let mut input = parsed.single("INPUT").unwrap_or_default().to_owned();
    let verbose = parsed.flag("VERBOSE");
    let patterns = parsed.multi("PATTERNS");
    if parsed.flag("LIST_ONLY") {
        if !carchive::list_tar(&input, patterns, verbose) {
            return Err(fatal(interp, &format!("file failed to list: {input}")));
        }
        return Ok(());
    }
    let mut destination = interp.current_binary_dir();
    if let Some(dest) = parsed.single("DESTINATION").filter(|d| !d.is_empty()) {
        destination = if is_full_path(dest) {
            dest.to_owned()
        } else {
            format!("{destination}/{dest}")
        };
        if mkdir(&destination).is_err() {
            return Err(fatal(
                interp,
                &format!("file failed to create directory: {destination}"),
            ));
        }
        if !is_full_path(&input) {
            let cwd = std::env::current_dir()
                .map(|dir| dir.to_string_lossy().into_owned())
                .unwrap_or_default();
            input = format!("{cwd}/{input}");
        }
    }
    // cmWorkingDirectory workdir(destDir).
    let previous = std::env::current_dir().ok();
    if std::env::set_current_dir(&destination).is_err() {
        return Err(fatal(
            interp,
            &format!("file Cannot change to directory \"{destination}\""),
        ));
    }
    let ok = carchive::extract_tar(
        &input,
        patterns,
        if parsed.flag("TOUCH") {
            ExtractTimestamps::No
        } else {
            ExtractTimestamps::Yes
        },
        verbose,
    );
    if let Some(previous) = previous {
        let _ = std::env::set_current_dir(previous);
    }
    if !ok {
        return Err(fatal(interp, &format!("file failed to extract: {input}")));
    }
    Ok(())
}
