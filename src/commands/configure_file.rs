//! cmConfigureFileCommand and the shared ConfigureString implementation.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use crate::interp::{CmdErr, CmdResult, Interp, err};
use crate::messages::MessageType;
use crate::util::is_off_opt;

use super::legacy_fs::{can_write, collapse, fatal, mode, raw_error, system_error};

pub fn register(interp: &mut Interp) {
    interp.register("configure_file", configure_file);
}

pub(super) fn newline_style(args: &[String]) -> Result<Option<&'static str>, &'static str> {
    let Some(index) = args.iter().position(|arg| arg == "NEWLINE_STYLE") else {
        return Ok(None);
    };
    match args.get(index + 1).map(String::as_str) {
        Some("LF" | "UNIX") => Ok(Some("\n")),
        Some("CRLF" | "DOS" | "WIN32") => Ok(Some("\r\n")),
        Some(_) => Err(
            "NEWLINE_STYLE sets an unknown style, only LF, CRLF, UNIX, DOS, and WIN32 are supported",
        ),
        None => Err("NEWLINE_STYLE must set a style: LF, CRLF, UNIX, DOS, or WIN32"),
    }
}

pub(super) fn configure_string(
    interp: &mut Interp,
    text: &str,
    at_only: bool,
    escape_quotes: bool,
) -> Result<String, String> {
    static DEFINE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"#([ \t]*)cmakedefine[ \t]+([A-Za-z_0-9]*)").unwrap());
    static DEFINE_01: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"#([ \t]*)cmakedefine01[ \t]+([A-Za-z_0-9]*)").unwrap());
    let mut output = String::new();
    for line in text.split_inclusive('\n') {
        let (line, newline) = line
            .strip_suffix('\n')
            .map_or((line, ""), |line| (line, "\n"));
        if let Some(captures) = DEFINE.captures(line) {
            let (indent, name) = (&captures[1], &captures[2]);
            if is_off_opt(interp.get_def(name)) {
                output.push_str(&format!("/* #undef {name} */"));
            } else {
                output.push_str(&line.replace(
                    &format!("#{indent}cmakedefine"),
                    &format!("#{indent}define"),
                ));
            }
        } else if let Some(captures) = DEFINE_01.captures(line) {
            let (indent, name) = (&captures[1], &captures[2]);
            output.push_str(&line.replace(
                &format!("#{indent}cmakedefine01"),
                &format!("#{indent}define"),
            ));
            output.push_str(if is_off_opt(interp.get_def(name)) {
                " 0"
            } else {
                " 1"
            });
        } else {
            output.push_str(line);
        }
        output.push_str(newline);
    }
    interp.expand_configure(&output, at_only, escape_quotes)
}

pub(super) fn configure_text(
    interp: &mut Interp,
    data: &[u8],
    newline: Option<&str>,
    force_newline: bool,
    at_only: bool,
    escape_quotes: bool,
) -> Vec<u8> {
    let mut output = Vec::new();
    for line in data.split_inclusive(|byte| *byte == b'\n') {
        let have_newline = line.ends_with(b"\n");
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        for chunk in line.utf8_chunks() {
            match configure_string(interp, chunk.valid(), at_only, escape_quotes) {
                Ok(text) => output.extend(text.as_bytes()),
                Err(message) => {
                    fatal(interp, &message);
                    output.extend(chunk.valid().as_bytes());
                }
            }
            output.extend(chunk.invalid());
        }
        if have_newline || force_newline || newline.is_some() {
            output.extend(newline.unwrap_or("\n").as_bytes());
        }
    }
    output
}

pub(super) fn bom(data: &[u8]) -> (usize, &'static str) {
    for (prefix, encoding) in [
        (&b"\xff\xfe\0\0"[..], "UTF-32LE"),
        (&b"\0\0\xfe\xff"[..], "UTF-32BE"),
        (&b"\xef\xbb\xbf"[..], "UTF-8"),
        (&b"\xff\xfe"[..], "UTF-16LE"),
        (&b"\xfe\xff"[..], "UTF-16BE"),
    ] {
        if data.starts_with(prefix) {
            return (prefix.len(), encoding);
        }
    }
    (0, "")
}

pub(super) fn write_configured(path: &str, data: &[u8]) -> io::Result<()> {
    let temp = format!("{path}.tmp");
    if let Some(parent) = Path::new(path).parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&temp, data)?;
    if fs::read(path).is_ok_and(|actual| actual == data) {
        fs::remove_file(temp)
    } else {
        let result = fs::rename(&temp, path);
        if result.is_err() {
            let _ = fs::remove_file(temp);
        }
        result
    }
}

fn configure_file(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 2 {
        return err("called with incorrect number of arguments, expected 2");
    }
    let input = collapse(&args[0], &interp.current_source_dir());
    if Path::new(&input).is_dir() {
        return err(format!(
            "input location\n  {input}\nis a directory but a file was expected."
        ));
    }
    let mut output = collapse(&args[1], &interp.current_binary_dir());
    if Path::new(&output).is_dir() {
        let name = args[0].rsplit('/').next().unwrap_or_default();
        output.push('/');
        output.push_str(name);
    }
    if !can_write(interp, &output) {
        interp.set_fatal();
        return err(format!(
            "attempted to configure a file: {output} into a source directory."
        ));
    }
    let newline = newline_style(args).map_err(|message| CmdErr::Error(message.into()))?;
    let mut copy_only = false;
    let mut escape_quotes = false;
    let mut at_only = false;
    let mut source_permissions = false;
    let mut default_permissions = false;
    let mut permissions = None;
    let mut in_permissions = false;
    let mut unknown = String::new();
    for arg in &args[2..] {
        match arg.as_str() {
            "COPYONLY" => {
                in_permissions = false;
                copy_only = true;
                if newline.is_some() {
                    return err("COPYONLY could not be used in combination with NEWLINE_STYLE");
                }
            }
            "ESCAPE_QUOTES" => {
                in_permissions = false;
                escape_quotes = true;
            }
            "@ONLY" => {
                in_permissions = false;
                at_only = true;
            }
            "NO_SOURCE_PERMISSIONS" | "USE_SOURCE_PERMISSIONS" => {
                if in_permissions {
                    return err(format!(
                        " given both FILE_PERMISSIONS and {arg}. Only one option allowed."
                    ));
                }
                source_permissions |= arg == "USE_SOURCE_PERMISSIONS";
                default_permissions |= arg == "NO_SOURCE_PERMISSIONS";
            }
            "FILE_PERMISSIONS" => {
                if source_permissions || default_permissions {
                    let key = if source_permissions {
                        "USE_SOURCE_PERMISSIONS"
                    } else {
                        "NO_SOURCE_PERMISSIONS"
                    };
                    return err(format!(
                        " given both FILE_PERMISSIONS and {key}. Only one option allowed."
                    ));
                }
                if permissions.is_none() {
                    permissions = Some(Vec::new());
                    in_permissions = true;
                }
            }
            "IMMEDIATE" | "NEWLINE_STYLE" | "LF" | "UNIX" | "CRLF" | "WIN32" | "DOS" => {}
            _ if in_permissions => permissions.as_mut().unwrap().push(arg),
            _ => unknown.push_str(&format!(" {arg}\n")),
        }
    }
    if !unknown.is_empty() {
        interp.issue(
            MessageType::AuthorWarning,
            &format!("configure_file called with unknown argument(s):\n{unknown}"),
        );
    }
    if source_permissions && default_permissions {
        return err(
            " given both USE_SOURCE_PERMISSIONS and NO_SOURCE_PERMISSIONS. Only one option allowed.",
        );
    }
    let mut bits = if default_permissions { 0o644 } else { 0 };
    if let Some(permissions) = permissions {
        if permissions.is_empty() {
            return err(" given FILE_PERMISSIONS without any options.");
        }
        let mut invalid = Vec::new();
        for permission in permissions {
            if let Some(mode) = mode(permission) {
                bits |= mode;
            } else {
                invalid.push(format!("\"{permission}\""));
            }
        }
        if !invalid.is_empty() {
            return err(format!(" given invalid permission {}.", invalid.join(",")));
        }
    }
    let data = match fs::read(&input) {
        Ok(data) => data,
        Err(error) => {
            let message = if error.kind() == io::ErrorKind::NotFound {
                format!("File {input} does not exist.")
            } else {
                format!("Could not open file for read in copy operation {input}")
            };
            raw_error(interp, &message);
            return err("Problem configuring file");
        }
    };
    interp.ext.configure_dependencies.insert(input.clone());
    // cmMakefile::AddCMakeDependFile appends the input to ListFiles, which
    // generators consume for the cmake re-run rule.
    interp.mf_mut().list_files.push(input.clone());
    let deps = interp
        .ext
        .misc
        .entry("cmake-depend-files".to_string())
        .or_default();
    if !deps.is_empty() {
        deps.push(';');
    }
    deps.push_str(&input);
    interp.ext.configure_outputs.insert(output.clone());
    if bits == 0 {
        bits = fs::metadata(&input).map_or(0o644, |meta| meta.permissions().mode());
    }
    let encoding = bom(&data).1;
    if !copy_only && !matches!(encoding, "" | "UTF-8") {
        return Err(fatal(
            interp,
            &format!("File starts with a Byte-Order-Mark that is not UTF-8:\n  {input}"),
        ));
    }
    let data = if copy_only {
        data
    } else {
        configure_text(interp, &data, newline, true, at_only, escape_quotes)
    };
    if let Err(error) = write_configured(&output, &data) {
        if copy_only {
            interp.issue(
                MessageType::FatalError,
                &format!("Fail to copy destinationfile: {}", system_error(&error)),
            );
        } else {
            raw_error(
                interp,
                &format!("Could not open file for write in copy operation {output}.tmp"),
            );
            eprintln!("System Error: {}", system_error(&error));
        }
        return err("Problem configuring file");
    }
    if let Err(error) = fs::set_permissions(&output, fs::Permissions::from_mode(bits)) {
        interp.issue(MessageType::FatalError, &system_error(&error));
        return err("Problem configuring file");
    }
    Ok(())
}
