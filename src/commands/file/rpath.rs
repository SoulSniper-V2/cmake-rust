//! Port of the `file(RPATH_*)`/`file(CHRPATH)`/`file(READ_ELF)` subcommands
//! (`cmFileCommand.cxx`) and the `cmSystemTools::*RPath` helpers they call
//! (`cmSystemTools.cxx`).

use std::fs::{self, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use crate::cmelf::{Elf, TAG_MIPS_RLD_MAP_REL, TAG_RPATH, TAG_RUNPATH};
use crate::interp::{CmdResult, Interp, err};
use crate::messages::MessageType;

/// `cmSystemTools::FileExists(file, true)`: exists and is not a directory.
fn file_exists(path: &str) -> bool {
    Path::new(path).is_file()
}

/// `cmFileTimes`: preserve access and modification times across a rewrite.
struct FileTimes {
    actime: i64,
    modtime: i64,
}

impl FileTimes {
    fn load(path: &str) -> Option<FileTimes> {
        fs::metadata(path).ok().map(|m| FileTimes {
            actime: m.atime(),
            modtime: m.mtime(),
        })
    }

    fn store(&self, path: &str) {
        let Ok(path) = std::ffi::CString::new(path) else {
            return;
        };
        let time = libc::utimbuf {
            actime: self.actime,
            modtime: self.modtime,
        };
        unsafe {
            libc::utime(path.as_ptr(), &time);
        }
    }
}

/// `FileModeGuard` from cmSystemTools.cxx: restores the file permission
/// bits if they changed while the file was open for update.
struct FileModeGuard {
    path: String,
    mode: u32,
    failed: bool,
}

impl FileModeGuard {
    fn new(path: &str, emsg: &mut String) -> FileModeGuard {
        match fs::metadata(path) {
            Ok(meta) => FileModeGuard {
                path: path.to_string(),
                mode: meta.mode() & 0o7777,
                failed: false,
            },
            Err(error) => {
                *emsg = format!("Cannot get file stat: {}", strerror(&error));
                FileModeGuard {
                    path: path.to_string(),
                    mode: 0,
                    failed: true,
                }
            }
        }
    }

    fn has_errors(&self) -> bool {
        self.failed
    }

    fn restore(&self, emsg: &mut String) -> bool {
        let meta = match fs::metadata(&self.path) {
            Ok(meta) => meta,
            Err(error) => {
                *emsg = format!("Cannot get file stat: {}", strerror(&error));
                return false;
            }
        };
        if meta.mode() & 0o7777 == self.mode {
            return true;
        }
        let Ok(path) = std::ffi::CString::new(self.path.clone()) else {
            return true;
        };
        if unsafe { libc::chmod(path.as_ptr(), self.mode) } != 0 {
            *emsg = format!(
                "Cannot restore the file mode: {}",
                strerror(&io::Error::last_os_error())
            );
            return false;
        }
        true
    }
}

fn strerror(error: &io::Error) -> String {
    match error.raw_os_error() {
        Some(code) => {
            let text = unsafe { std::ffi::CStr::from_ptr(libc::strerror(code)) };
            text.to_string_lossy().into_owned()
        }
        None => error.to_string(),
    }
}

/// `cmSystemToolsFindRPath`: find `want` as a colon-separated component of
/// `have`. Returns the byte offset or None.
fn find_rpath(have: &str, want: &str) -> Option<usize> {
    let have = have.as_bytes();
    let want = want.as_bytes();
    let mut pos = 0;
    while pos < have.len() {
        let beg = have[pos..]
            .windows(want.len())
            .position(|w| w == want)
            .map(|o| pos + o)
            .or(if want.is_empty() { Some(pos) } else { None })?;
        if beg > 0 && have[beg - 1] != b':' {
            pos = beg + 1;
            continue;
        }
        let end = beg + want.len();
        if end < have.len() && have[end] != b':' {
            pos = beg + 1;
            continue;
        }
        return Some(beg);
    }
    None
}

struct RPathInfo {
    position: u64,
    size: u64,
    name: &'static str,
    value: String,
}

/// `AdjustRPathELF`. Returns None when the file is not a valid ELF, Some of
/// the success flag otherwise.
fn adjust_rpath_elf(
    file: &str,
    empty: impl FnOnce(&mut String, &Elf) -> bool,
    adjust: impl Fn(&mut Option<String>, &str, &'static str, &mut String) -> bool,
    emsg: &mut String,
    changed: &mut bool,
) -> Option<bool> {
    *changed = false;
    let mut rp: Vec<RPathInfo> = Vec::new();
    let mut should_remove = true;
    {
        let mut elf = Elf::open(file);
        if !elf.is_valid() {
            return None;
        }
        if !elf.has_dynamic_section() {
            return Some(true);
        }
        let mut se: Vec<(crate::cmelf::StringEntry, &'static str)> = Vec::new();
        if let Some(entry) = elf.get_rpath() {
            se.push((entry, "RPATH"));
        }
        if let Some(entry) = elf.get_runpath() {
            se.push((entry, "RUNPATH"));
        }
        if se.is_empty() {
            return Some(empty(emsg, &elf));
        }
        for (entry, name) in se {
            // If both RPATH and RUNPATH refer to the same string literal it
            // needs to be changed only once.
            if rp.iter().any(|r| r.position == entry.position) {
                continue;
            }
            let mut out: Option<String> = None;
            if !adjust(&mut out, &entry.value, name, emsg) {
                return Some(false);
            }
            match out {
                Some(value) => {
                    if !value.is_empty() {
                        should_remove = false;
                    }
                    if entry.size < value.len() as u64 + 1 {
                        *emsg = format!("The replacement path is too long for the {name} entry.");
                        return Some(false);
                    }
                    rp.push(RPathInfo {
                        position: entry.position,
                        size: entry.size,
                        name,
                        value,
                    });
                }
                None => {
                    should_remove = false;
                }
            }
        }
    }

    if rp.is_empty() {
        return Some(true);
    }

    // If the resulting rpath is empty, remove the entire entry instead.
    if should_remove {
        return remove_rpath(file, emsg, changed);
    }

    let guard = FileModeGuard::new(file, emsg);
    if guard.has_errors() {
        return Some(false);
    }

    {
        let Ok(mut f) = OpenOptions::new().read(true).write(true).open(file) else {
            *emsg = "Error opening file for update.".to_string();
            return Some(false);
        };
        for entry in &rp {
            if f.seek(SeekFrom::Start(entry.position)).is_err() {
                *emsg = format!("Error seeking to {} position.", entry.name);
                return Some(false);
            }
            let mut bytes = entry.value.clone().into_bytes();
            bytes.resize(entry.size as usize, 0);
            if f.write_all(&bytes).is_err() {
                *emsg = format!("Error writing the new {} string to the file.", entry.name);
                return Some(false);
            }
        }
    }

    if !guard.restore(emsg) {
        return Some(false);
    }
    *changed = true;
    Some(true)
}

/// `RemoveRPathELF`.
fn remove_rpath(file: &str, emsg: &mut String, removed: &mut bool) -> Option<bool> {
    *removed = false;
    let (zero, bytes, bytes_begin) = {
        let mut zero: Vec<(u64, u64)> = Vec::new();
        let mut elf = Elf::open(file);
        if !elf.is_valid() {
            return None;
        }
        let mut se: Vec<crate::cmelf::StringEntry> = Vec::new();
        if let Some(entry) = elf.get_rpath() {
            se.push(entry);
        }
        if let Some(entry) = elf.get_runpath() {
            se.push(entry);
        }
        if se.is_empty() {
            // There is no RPATH or RUNPATH anyway.
            return Some(true);
        }
        if se.len() == 2 && se[1].index_in_section < se[0].index_in_section {
            se.swap(0, 1);
        }
        let mut dentries = elf.get_dynamic_entries();
        if dentries.is_empty() {
            *emsg = "DYNAMIC section contains a DT_NULL before the end.".to_string();
            return Some(false);
        }
        for entry in &se {
            zero.push((entry.position, entry.size));
        }
        let sizeof_dentry = elf
            .get_dynamic_entry_position(1)
            .wrapping_sub(elf.get_dynamic_entry_position(0));

        let mut erased = 0u64;
        let is_mips = elf.is_mips();
        dentries.retain_mut(|entry| {
            if entry.0 == TAG_RPATH || entry.0 == TAG_RUNPATH {
                erased += 1;
                return false;
            }
            if entry.0 == TAG_MIPS_RLD_MAP_REL && is_mips {
                // Keep DT_MIPS_RLD_MAP_REL pointing at the same place after
                // earlier entries shrink the table (see cmSystemTools.cxx).
                entry.1 = entry.1.wrapping_add(erased.wrapping_mul(sizeof_dentry));
            }
            true
        });
        let bytes = elf.encode_dynamic_entries(&dentries);
        let bytes_begin = elf.get_dynamic_entry_position(0);
        (zero, bytes, bytes_begin)
    };

    let guard = FileModeGuard::new(file, emsg);
    if guard.has_errors() {
        return Some(false);
    }

    {
        let Ok(mut f) = OpenOptions::new().read(true).write(true).open(file) else {
            *emsg = "Error opening file for update.".to_string();
            return Some(false);
        };
        if f.seek(SeekFrom::Start(bytes_begin)).is_err() {
            *emsg = "Error seeking to DYNAMIC table header for RPATH.".to_string();
            return Some(false);
        }
        if f.write_all(&bytes).is_err() {
            *emsg = "Error replacing DYNAMIC table header.".to_string();
            return Some(false);
        }
        for &(position, size) in &zero {
            if f.seek(SeekFrom::Start(position)).is_err() {
                *emsg = "Error seeking to RPATH position.".to_string();
                return Some(false);
            }
            if f.write_all(&vec![0u8; size as usize]).is_err() {
                *emsg = "Error writing the empty rpath string to the file.".to_string();
                return Some(false);
            }
        }
    }

    if !guard.restore(emsg) {
        return Some(false);
    }
    *removed = true;
    Some(true)
}

/// `MakeEmptyCallback`.
fn empty_callback(new_rpath: &str, emsg: &mut String, elf: &Elf) -> bool {
    if new_rpath.is_empty() {
        // The new rpath is empty and there is no rpath anyway so it is
        // okay.
        return true;
    }
    *emsg = format!(
        "No valid ELF RPATH or RUNPATH entry exists in the file; {}",
        elf.error_message()
    );
    false
}

/// `cmSystemTools::ChangeRPath` (ELF only; XCOFF is unsupported upstream on
/// this platform so an unrecognized file falls through to the same result).
fn change_rpath(
    file: &str,
    old_rpath: &str,
    new_rpath: &str,
    remove_environment_rpath: bool,
    emsg: &mut String,
    changed: &mut bool,
) -> bool {
    let adjust =
        |out: &mut Option<String>, in_rpath: &str, se_name: &'static str, emsg2: &mut String| {
            let Some(pos) = find_rpath(in_rpath, old_rpath) else {
                // If it contains the new rpath instead then it is okay.
                if find_rpath(in_rpath, new_rpath).is_some() {
                    return true;
                }
                *emsg2 = format!(
                    "The current {se_name} is:\n  {in_rpath}\nwhich does not contain:\n  \
                 {old_rpath}\nas was expected."
                );
                return false;
            };
            let mut prefix_len = pos;
            // If oldRPath was at the end of the file's RPath, and newRPath is
            // empty, remove the unnecessary ':' at the end.
            if new_rpath.is_empty()
                && pos > 0
                && in_rpath.as_bytes()[pos - 1] == b':'
                && pos + old_rpath.len() == in_rpath.len()
            {
                prefix_len -= 1;
            }
            let mut value = String::new();
            if !remove_environment_rpath {
                value.push_str(&in_rpath[..prefix_len]);
            }
            value.push_str(new_rpath);
            value.push_str(&in_rpath[pos + old_rpath.len()..]);
            *out = Some(value);
            true
        };
    match adjust_rpath_elf(
        file,
        |emsg, elf| empty_callback(new_rpath, emsg, elf),
        adjust,
        emsg,
        changed,
    ) {
        Some(result) => result,
        None => {
            // The file format is not recognized. Assume it has no RPATH.
            if new_rpath.is_empty() {
                return true;
            }
            *emsg = "The file format is not recognized.".to_string();
            false
        }
    }
}

/// `cmSystemTools::SetRPath`.
fn set_rpath(file: &str, new_rpath: &str, emsg: &mut String, changed: &mut bool) -> bool {
    let adjust =
        |out: &mut Option<String>, in_rpath: &str, _se_name: &'static str, _emsg: &mut String| {
            if in_rpath != new_rpath {
                *out = Some(new_rpath.to_string());
            }
            true
        };
    match adjust_rpath_elf(
        file,
        |emsg, elf| empty_callback(new_rpath, emsg, elf),
        adjust,
        emsg,
        changed,
    ) {
        Some(result) => result,
        None => {
            if new_rpath.is_empty() {
                return true;
            }
            *emsg = "The file format is not recognized.".to_string();
            false
        }
    }
}

/// `cmSystemTools::RemoveRPath`.
fn cm_remove_rpath(file: &str, emsg: &mut String, removed: &mut bool) -> bool {
    // The file format is not recognized. Assume it has no RPATH.
    remove_rpath(file, emsg, removed).unwrap_or(true)
}

/// `cmSystemTools::CheckRPath`.
fn check_rpath(file: &str, new_rpath: &str) -> bool {
    let mut elf = Elf::open(file);
    if elf.is_valid() {
        let se = elf.get_rpath().or_else(|| elf.get_runpath());
        if new_rpath.is_empty() {
            return se.is_none();
        }
        return se.is_some_and(|se| find_rpath(&se.value, new_rpath).is_some());
    }
    // The file format is not recognized. Assume it has no RPATH.
    new_rpath.is_empty()
}

// ---------------------------------------------------------------------------
// Argument parsing shared by the handlers, following cmArgumentParser for
// the keyword shapes used here.

struct Parsed {
    values: std::collections::HashMap<&'static str, String>,
    flags: std::collections::HashMap<&'static str, bool>,
    unknown: Vec<String>,
    keyword_errors: Vec<String>,
}

fn parse(args: &[String], single: &[&'static str], flags: &[&'static str]) -> Parsed {
    let mut parsed = Parsed {
        values: std::collections::HashMap::new(),
        flags: std::collections::HashMap::new(),
        unknown: Vec::new(),
        keyword_errors: Vec::new(),
    };
    let mut pending: Option<&'static str> = None;
    for arg in &args[1..] {
        let keyword = single
            .iter()
            .chain(flags.iter())
            .find(|k| **k == arg.as_str());
        if let Some(&keyword) = keyword {
            if let Some(previous) = pending.take() {
                parsed.keyword_errors.push(format!(
                    "Error after keyword \"{previous}\":\n  missing required value\n"
                ));
            }
            if single.contains(&keyword) {
                pending = Some(keyword);
            } else {
                parsed.flags.insert(keyword, true);
            }
            continue;
        }
        match pending.take() {
            Some(keyword) => {
                parsed.values.insert(keyword, arg.clone());
            }
            None => parsed.unknown.push(arg.clone()),
        }
    }
    if let Some(previous) = pending.take() {
        parsed.keyword_errors.push(format!(
            "Error after keyword \"{previous}\":\n  missing required value\n"
        ));
    }
    parsed
}

/// `ParseResult::MaybeReportError`: issue FATAL_ERROR and return true.
fn maybe_report_error(interp: &mut Interp, parsed: &Parsed) -> bool {
    if parsed.keyword_errors.is_empty() {
        return false;
    }
    let error = parsed.keyword_errors.concat();
    interp.issue(MessageType::FatalError, &error);
    interp.set_fatal();
    true
}

fn status_line(interp: &mut Interp, message: &str) {
    // cmMakefile::DisplayStatus -> cmake::UpdateProgress prints
    // "-- <message>" unconditionally outside find-package mode.
    crate::commands::message::status_line(interp, message);
}

/// `HandleRPathChangeCommand` (RPATH_CHANGE and CHRPATH).
fn rpath_change(interp: &mut Interp, args: &[String]) -> CmdResult {
    let parsed = parse(
        args,
        &["FILE", "OLD_RPATH", "NEW_RPATH"],
        &["INSTALL_REMOVE_ENVIRONMENT_RPATH"],
    );
    if let Some(unknown) = parsed.unknown.first() {
        return err(format!("RPATH_CHANGE given unknown argument {unknown}"));
    }
    if maybe_report_error(interp, &parsed) {
        return Ok(());
    }
    let file = parsed.values.get("FILE").cloned().unwrap_or_default();
    if file.is_empty() {
        return err("RPATH_CHANGE not given FILE option.");
    }
    let Some(old_rpath) = parsed.values.get("OLD_RPATH") else {
        return err("RPATH_CHANGE not given OLD_RPATH option.");
    };
    let Some(new_rpath) = parsed.values.get("NEW_RPATH") else {
        return err("RPATH_CHANGE not given NEW_RPATH option.");
    };
    if !file_exists(&file) {
        return err(format!(
            "RPATH_CHANGE given FILE \"{file}\" that does not exist."
        ));
    }
    let times = FileTimes::load(&file);
    let mut emsg = String::new();
    let mut changed = false;
    if !change_rpath(
        &file,
        old_rpath,
        new_rpath,
        parsed
            .flags
            .contains_key("INSTALL_REMOVE_ENVIRONMENT_RPATH"),
        &mut emsg,
        &mut changed,
    ) {
        return err(format!(
            "RPATH_CHANGE could not write new RPATH:\n  {new_rpath}\nto the file:\n  \
             {file}\n{emsg}"
        ));
    }
    if changed {
        status_line(
            interp,
            &format!("Set non-toolchain portion of runtime path of \"{file}\" to \"{new_rpath}\""),
        );
    }
    if let Some(times) = times {
        times.store(&file);
    }
    Ok(())
}

/// `HandleRPathSetCommand` (RPATH_SET).
fn rpath_set(interp: &mut Interp, args: &[String]) -> CmdResult {
    let parsed = parse(args, &["FILE", "NEW_RPATH"], &[]);
    if let Some(unknown) = parsed.unknown.first() {
        return err(format!(
            "RPATH_SET given unrecognized argument \"{unknown}\"."
        ));
    }
    if maybe_report_error(interp, &parsed) {
        return Ok(());
    }
    let file = parsed.values.get("FILE").cloned().unwrap_or_default();
    if file.is_empty() {
        return err("RPATH_SET not given FILE option.");
    }
    let Some(new_rpath) = parsed.values.get("NEW_RPATH") else {
        return err("RPATH_SET not given NEW_RPATH option.");
    };
    if !file_exists(&file) {
        return err(format!(
            "RPATH_SET given FILE \"{file}\" that does not exist."
        ));
    }
    let times = FileTimes::load(&file);
    let mut emsg = String::new();
    let mut changed = false;
    if !set_rpath(&file, new_rpath, &mut emsg, &mut changed) {
        return err(format!(
            "RPATH_SET could not write new RPATH:\n  {new_rpath}\nto the file:\n  \
             {file}\n{emsg}"
        ));
    }
    if changed {
        status_line(
            interp,
            &format!("Set non-toolchain portion of runtime path of \"{file}\" to \"{new_rpath}\""),
        );
    }
    if let Some(times) = times {
        times.store(&file);
    }
    Ok(())
}

/// `HandleRPathRemoveCommand` (RPATH_REMOVE).
fn rpath_remove(interp: &mut Interp, args: &[String]) -> CmdResult {
    let parsed = parse(args, &["FILE"], &[]);
    if let Some(unknown) = parsed.unknown.first() {
        return err(format!("RPATH_REMOVE given unknown argument {unknown}"));
    }
    if maybe_report_error(interp, &parsed) {
        return Ok(());
    }
    let file = parsed.values.get("FILE").cloned().unwrap_or_default();
    if file.is_empty() {
        return err("RPATH_REMOVE not given FILE option.");
    }
    if !file_exists(&file) {
        return err(format!(
            "RPATH_REMOVE given FILE \"{file}\" that does not exist."
        ));
    }
    let times = FileTimes::load(&file);
    let mut emsg = String::new();
    let mut removed = false;
    if !cm_remove_rpath(&file, &mut emsg, &mut removed) {
        return err(format!(
            "RPATH_REMOVE could not remove RPATH from file: \n  {file}\n{emsg}"
        ));
    }
    if removed {
        status_line(interp, &format!("Removed runtime path from \"{file}\""));
    }
    if let Some(times) = times {
        times.store(&file);
    }
    Ok(())
}

/// `HandleRPathCheckCommand` (RPATH_CHECK).
fn rpath_check(interp: &mut Interp, args: &[String]) -> CmdResult {
    let parsed = parse(args, &["FILE", "RPATH"], &[]);
    if let Some(unknown) = parsed.unknown.first() {
        return err(format!("RPATH_CHECK given unknown argument {unknown}"));
    }
    if maybe_report_error(interp, &parsed) {
        return Ok(());
    }
    let file = parsed.values.get("FILE").cloned().unwrap_or_default();
    if file.is_empty() {
        return err("RPATH_CHECK not given FILE option.");
    }
    let Some(rpath) = parsed.values.get("RPATH") else {
        return err("RPATH_CHECK not given RPATH option.");
    };
    // If the file exists but does not have the desired RPath then delete
    // it, so install re-copies a file whose RPath will change.
    if file_exists(&file) && !check_rpath(&file, rpath) {
        let _ = fs::remove_file(&file);
    }
    Ok(())
}

/// `HandleReadElfCommand` (READ_ELF).
fn read_elf(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 4 {
        return err("READ_ELF must be called with at least three additional arguments.");
    }
    let file = args[1].clone();
    // Unrecognized arguments are dropped (unparsedArguments is nullptr)
    // and keyword errors are never reported (no MaybeReportError call).
    let parsed = parse(&args[1..], &["RPATH", "RUNPATH", "CAPTURE_ERROR"], &[]);
    if !file_exists(&file) {
        return err(format!(
            "READ_ELF given FILE \"{file}\" that does not exist."
        ));
    }
    let mut elf = Elf::open(&file);
    if !elf.is_valid() {
        match parsed.values.get("CAPTURE_ERROR") {
            Some(var) if !var.is_empty() => {
                interp.set_def(var.clone(), "not a valid ELF file".to_string());
                return Ok(());
            }
            _ => {
                return err(format!(
                    "READ_ELF given FILE:\n  {file}\nthat is not a valid ELF file."
                ));
            }
        }
    }
    if let Some(var) = parsed.values.get("RPATH").filter(|v| !v.is_empty()) {
        if let Some(se) = elf.get_rpath() {
            interp.set_def(var.clone(), se.value.replace(':', ";"));
        }
    }
    if let Some(var) = parsed.values.get("RUNPATH").filter(|v| !v.is_empty()) {
        if let Some(se) = elf.get_runpath() {
            interp.set_def(var.clone(), se.value.replace(':', ";"));
        }
    }
    Ok(())
}

pub fn run(interp: &mut Interp, args: &[String]) -> CmdResult {
    match args[0].as_str() {
        "RPATH_CHANGE" | "CHRPATH" => rpath_change(interp, args),
        "RPATH_SET" => rpath_set(interp, args),
        "RPATH_REMOVE" => rpath_remove(interp, args),
        "RPATH_CHECK" => rpath_check(interp, args),
        "READ_ELF" => read_elf(interp, args),
        _ => unreachable!(),
    }
}
