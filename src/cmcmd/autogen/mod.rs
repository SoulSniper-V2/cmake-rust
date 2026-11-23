//! `cmake -E cmake_autogen` / `cmake -E cmake_autorcc`: the Qt AUTOGEN
//! build-time runtime (`cmQtAutoMocUic.cxx`, `cmQtAutoRcc.cxx` and their
//! shared `cmQtAutoGen.cxx` / `cmQtAutoGenerator.cxx` support).

mod filetime;
mod generator;
mod libmap;
mod mocuic;
mod rcc;

use std::collections::BTreeSet;
use std::process::{Command, Stdio};

/// `cmQtAutoMocUic`: `cmake -E cmake_autogen <info> <config> [<exec-config>]`.
pub(crate) fn moc_uic(info_file: &str, config: &str, executable_config: &str) -> bool {
    mocuic::run(info_file, config, executable_config)
}

/// `cmQtAutoRcc`: `cmake -E cmake_autorcc <info> [<config> [<exec-config>]]`.
pub(crate) fn rcc(info_file: &str, config: &str, executable_config: &str) -> bool {
    rcc::run(info_file, config, executable_config)
}

/// `cmQtAutoGen::GenT`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum GenT {
    Gen,
    Moc,
    Uic,
    Rcc,
}

impl GenT {
    /// `cmQtAutoGen::GeneratorName`.
    fn name(self) -> &'static str {
        match self {
            GenT::Gen => "AutoGen",
            GenT::Moc => "AutoMoc",
            GenT::Uic => "AutoUic",
            GenT::Rcc => "AutoRcc",
        }
    }
}

/// `cmQtAutoGen::IntegerVersion`.
#[derive(Clone, Copy, Default)]
pub(crate) struct IntegerVersion {
    pub(crate) major: u32,
    pub(crate) minor: u32,
}

impl IntegerVersion {
    fn ge(self, major: u32, minor: u32) -> bool {
        self.major > major || (self.major == major && self.minor >= minor)
    }
}

/// `cmQtAutoGen::Quoted`: escape and enclose in double quotes.
pub(crate) fn quoted(text: &str) -> String {
    let mut res = text.to_owned();
    for (from, to) in [
        ("\\", "\\\\"),
        ("\"", "\\\""),
        ("\u{7}", "\\a"),
        ("\u{8}", "\\b"),
        ("\u{c}", "\\f"),
        ("\n", "\\n"),
        ("\r", "\\r"),
        ("\t", "\\t"),
        ("\u{b}", "\\v"),
    ] {
        res = res.replace(from, to);
    }
    format!("\"{res}\"")
}

/// `cmQtAutoGen::QuotedCommand`.
pub(crate) fn quoted_command(command: &[String]) -> String {
    let mut res = String::new();
    for item in command {
        if !res.is_empty() {
            res.push(' ');
        }
        let cesc = quoted(item);
        if item.is_empty() || cesc.len() > item.len() + 2 || cesc.contains(' ') {
            res.push_str(&cesc);
        } else {
            res.push_str(item);
        }
    }
    res
}

/// `cmQtAutoGen::ParentDir` (unlike `GetFilenamePath`, `/a` has no parent).
pub(crate) fn parent_dir(filename: &str) -> String {
    match filename.rfind('/') {
        Some(pos) => filename[..pos].to_owned(),
        None => String::new(),
    }
}

/// `cmQtAutoGen::SubDirPrefix`.
pub(crate) fn sub_dir_prefix(filename: &str) -> String {
    match filename.rfind('/') {
        Some(pos) => filename[..=pos].to_owned(),
        None => String::new(),
    }
}

/// `cmQtAutoGen::AppendFilenameSuffix`: the last `.` in the *whole* name.
pub(crate) fn append_filename_suffix(filename: &str, suffix: &str) -> String {
    match filename.rfind('.') {
        Some(pos) => format!("{}{}{}", &filename[..pos], suffix, &filename[pos..]),
        None => format!("{filename}{suffix}"),
    }
}

/// `MergeOptions` from `cmQtAutoGen.cxx`.
fn merge_options(base: &mut Vec<String>, new: &[String], value_opts: &[&str], qt5_or_later: bool) {
    if new.is_empty() {
        return;
    }
    if base.is_empty() {
        *base = new.to_vec();
        return;
    }
    let mut extra = Vec::new();
    let mut fit = 0;
    while fit < new.len() {
        let opt = &new[fit];
        if let Some(exist) = base.iter().position(|b| b == opt) {
            if opt.len() >= 2 {
                // Acquire the option name
                let bytes = opt.as_bytes();
                let mut name_start = 0;
                if bytes[0] == b'-' {
                    name_start = 1;
                    if qt5_or_later && bytes[1] == b'-' {
                        name_start = 2;
                    }
                }
                let opt_name = &opt[name_start..];
                // Test if this is a value option and change the existing value
                if !opt_name.is_empty()
                    && value_opts.contains(&opt_name)
                    && exist + 1 < base.len()
                    && fit + 1 < new.len()
                {
                    base[exist + 1] = new[fit + 1].clone();
                    fit += 1;
                }
            }
        } else {
            extra.push(opt.clone());
        }
        fit += 1;
    }
    base.extend(extra);
}

/// `cmQtAutoGen::UicMergeOptions`.
pub(crate) fn uic_merge_options(base: &mut Vec<String>, new: &[String], qt5_or_later: bool) {
    merge_options(
        base,
        new,
        &["tr", "translate", "postfix", "generator", "include", "g"],
        qt5_or_later,
    );
}

/// `cmQtAutoGen::RccMergeOptions` (kept for parity; unused by the runtime).
#[allow(dead_code)]
pub(crate) fn rcc_merge_options(base: &mut Vec<String>, new: &[String], qt5_or_later: bool) {
    merge_options(
        base,
        new,
        &["name", "root", "compress", "threshold"],
        qt5_or_later,
    );
}

/// `RccListParseContent`: extract `<file>path</file>` entries.
fn rcc_list_parse_content(content: &str, files: &mut Vec<String>) {
    let file_match = crate::cmregex::Regex::new("(<file[^<]+)").unwrap();
    let file_replace = crate::cmregex::Regex::new("(^<file[^>]*>)").unwrap();
    let bytes = content.as_bytes();
    let mut offset = 0;
    while let Some(m) = file_match.find_bytes(bytes, offset, false) {
        let entry = m.groups[1].unwrap_or_default();
        let entry_text = &content[entry.0..entry.1];
        // The entry always starts with the "<file..." tag.
        let tag_len = file_replace
            .find(entry_text)
            .and_then(|m| m.groups[1])
            .map_or(0, |(s, e)| e - s);
        files.push(entry_text[tag_len..].to_owned());
        offset = entry.1;
    }
}

/// `RccListParseOutput`: rcc `--list` output and `RCC: Error in` recovery.
fn rcc_list_parse_output(
    stdout: &str,
    stderr: &str,
    files: &mut Vec<String>,
    error: &mut String,
) -> bool {
    fn strip_cr(line: &str) -> &str {
        line.split('\r').next().unwrap_or("")
    }
    for line in stdout.lines() {
        let line = strip_cr(line);
        if !line.is_empty() {
            files.push(line.to_owned());
        }
    }
    for line in stderr.lines() {
        let line = strip_cr(line);
        if let Some(rest) = line.strip_prefix("RCC: Error in") {
            let search = "Cannot find file '";
            let Some(pos) = rest.find(search) else {
                *error = format!("rcc lists unparsable output:\n{}\n", quoted(line));
                return false;
            };
            let name_start = pos + search.len();
            if name_start < line.len() {
                files.push(line[name_start..line.len() - 1].to_owned());
            }
        }
    }
    true
}

/// `cmQtAutoGen::RccLister`.
pub(crate) struct RccLister {
    pub(crate) executable: String,
    pub(crate) list_options: Vec<String>,
}

impl RccLister {
    /// `RccLister::list`: absolute resource paths appended to `files`.
    pub(crate) fn list(
        &self,
        qrc_file: &str,
        files: &mut Vec<String>,
        error: &mut String,
        verbose: bool,
    ) -> bool {
        if !std::path::Path::new(qrc_file).is_file() {
            *error = format!("The resource file {} does not exist.", quoted(qrc_file));
            return false;
        }
        // Run rcc list in the directory of the qrc file with the pathless
        // qrc file name argument so rcc prints relative paths.
        let file_dir = crate::util::filename_path(qrc_file);
        if !self.executable.is_empty()
            && std::path::Path::new(&self.executable).is_file()
            && !self.list_options.is_empty()
        {
            let mut cmd = vec![self.executable.clone()];
            cmd.extend(self.list_options.iter().cloned());
            cmd.push(crate::util::filename_name(qrc_file));
            if verbose {
                print!("Running command:\n{}\n", quoted_command(&cmd));
                use std::io::Write as _;
                let _ = std::io::stdout().flush();
            }
            let output = Command::new(&cmd[0])
                .args(&cmd[1..])
                .current_dir(if file_dir.is_empty() { "." } else { &file_dir })
                .stdin(Stdio::null())
                .output();
            let (ok, code, rcc_stdout, rcc_stderr) = match output {
                Ok(out) => (
                    true,
                    out.status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&out.stdout).into_owned(),
                    String::from_utf8_lossy(&out.stderr).into_owned(),
                ),
                Err(_) => (false, 0, String::new(), String::new()),
            };
            if !ok || code != 0 {
                *error = format!("The rcc list process failed for {}\n", quoted(qrc_file));
                if !rcc_stdout.is_empty() {
                    error.push_str(&rcc_stdout);
                    error.push('\n');
                }
                if !rcc_stderr.is_empty() {
                    error.push_str(&rcc_stderr);
                    error.push('\n');
                }
                return false;
            }
            if !rcc_list_parse_output(&rcc_stdout, &rcc_stderr, files, error) {
                return false;
            }
        } else {
            // We can't use rcc for the file listing. Parse the qrc content.
            match std::fs::read(qrc_file) {
                Ok(bytes) => rcc_list_parse_content(&String::from_utf8_lossy(&bytes), files),
                _ => {
                    *error = format!("The resource file {} is not readable\n", quoted(qrc_file));
                    return false;
                }
            }
        }
        // Convert relative paths to absolute paths
        for entry in files.iter_mut() {
            *entry = crate::util::collapse_full_path(entry, &file_dir);
        }
        true
    }
}

/// `cmStrToULong` (leading whitespace, no `-`, full consume).
pub(crate) fn str_to_ulong(value: &str) -> Option<u64> {
    let trimmed = value.trim_start_matches(|c: char| c.is_ascii_whitespace());
    if trimmed.starts_with('-') {
        return None;
    }
    let (parsed, ok) = crate::util::strtol(trimmed, 10);
    ok.then_some(parsed as u64)
}

/// `std::set<std::string>` ordered insert.
pub(crate) fn sorted_keys(source: &BTreeSet<String>, base_prefix: usize) -> Vec<IncludeKey> {
    source
        .iter()
        .map(|key| IncludeKey::new(key.clone(), base_prefix))
        .collect()
}

/// `cmQtAutoMocUicT::IncludeKeyT`.
#[derive(Clone)]
pub(crate) struct IncludeKey {
    /// Full include string.
    pub(crate) key: String,
    /// Include directory (with trailing `/` or empty).
    pub(crate) dir: String,
    /// Include base name without directory, extension and prefix.
    pub(crate) base: String,
}

impl IncludeKey {
    pub(crate) fn new(key: String, base_prefix: usize) -> IncludeKey {
        let dir = sub_dir_prefix(&key);
        let mut base = crate::util::filename_without_last_extension(&key);
        if base_prefix != 0 {
            // `std::string::substr` throws past the end; clamp instead.
            base = base.get(base_prefix..).unwrap_or("").to_owned();
        }
        IncludeKey { key, dir, base }
    }
}
