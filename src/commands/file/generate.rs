//! `file(GENERATE)` (cmFileCommand HandleGenerateCommand); evaluation happens
//! at generate time in `generator::evaluation_files`.

use crate::interp::{CmdResult, Interp, err};
use crate::state::EvaluationFile;

pub fn run(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 5 {
        return err("Incorrect arguments to GENERATE subcommand.");
    }
    const KEYWORDS: [&str; 9] = [
        "OUTPUT",
        "INPUT",
        "CONTENT",
        "CONDITION",
        "TARGET",
        "NO_SOURCE_PERMISSIONS",
        "USE_SOURCE_PERMISSIONS",
        "FILE_PERMISSIONS",
        "NEWLINE_STYLE",
    ];
    let mut values: std::collections::HashMap<&str, String> = std::collections::HashMap::new();
    let mut parsed: Vec<&str> = Vec::new();
    let mut permissions_list: Vec<String> = Vec::new();
    let mut no_source = false;
    let mut use_source = false;
    let mut unparsed = false;
    let mut current: Option<&str> = None;
    let mut missing: Vec<&str> = Vec::new();
    for arg in &args[1..] {
        if let Some(keyword) = KEYWORDS.iter().find(|keyword| **keyword == arg.as_str()) {
            if let Some(previous) = current
                && previous != "FILE_PERMISSIONS"
            {
                missing.push(previous);
            }
            parsed.push(keyword);
            current = None;
            match *keyword {
                "NO_SOURCE_PERMISSIONS" => no_source = true,
                "USE_SOURCE_PERMISSIONS" => use_source = true,
                _ => current = Some(keyword),
            }
            continue;
        }
        match current {
            Some("FILE_PERMISSIONS") => permissions_list.push(arg.clone()),
            Some(keyword) => {
                values.insert(keyword, arg.clone());
                current = None;
            }
            None => unparsed = true,
        }
    }
    if let Some(previous) = current
        && (previous != "FILE_PERMISSIONS" || permissions_list.is_empty())
    {
        missing.push(previous);
    }
    if !missing.is_empty() {
        // cmArgumentParser's keyword error retains its trailing newline,
        // which the documentation formatter turns into a blank line before
        // the call stack.
        let text = format!(
            "Error after keyword \"{}\":\n  missing required value\n",
            missing[0]
        );
        interp.issue(crate::messages::MessageType::FatalError, &text);
        interp.set_fatal_if_script();
        return Ok(());
    }
    if unparsed {
        return err("Unknown argument to GENERATE subcommand.");
    }
    let Some(output) = values
        .get("OUTPUT")
        .filter(|_| parsed.first() == Some(&"OUTPUT"))
        .cloned()
    else {
        return err("GENERATE requires OUTPUT as first option.");
    };
    if !values.contains_key("INPUT") && !values.contains_key("CONTENT") {
        return err("GENERATE requires INPUT or CONTENT option.");
    }
    let input_is_content = parsed.get(1) == Some(&"CONTENT");
    if !input_is_content && parsed.get(1) != Some(&"INPUT") {
        return err("Unknown argument to GENERATE subcommand.");
    }
    let input = if input_is_content {
        values["CONTENT"].clone()
    } else {
        values["INPUT"].clone()
    };
    let condition = values.get("CONDITION").cloned().unwrap_or_default();
    if values.contains_key("CONDITION") && condition.is_empty() {
        return err("CONDITION of sub-command GENERATE must not be empty if specified.");
    }
    let target = values.get("TARGET").cloned().unwrap_or_default();
    if values.contains_key("TARGET") && target.is_empty() {
        return err("TARGET of sub-command GENERATE must not be empty if specified.");
    }
    let newline = match values.get("NEWLINE_STYLE").map(String::as_str) {
        None => String::new(),
        Some("LF" | "UNIX") => "\n".to_owned(),
        Some("CRLF" | "WIN32" | "DOS") => "\r\n".to_owned(),
        Some(_) => {
            // cmNewLineStyle::ReadFromArguments.
            return err("GENERATE NEWLINE_STYLE sets an unknown style, only LF, CRLF, UNIX, DOS, and WIN32 are supported".to_owned());
        }
    };
    if no_source && use_source {
        return err(
            "given both NO_SOURCE_PERMISSIONS and USE_SOURCE_PERMISSIONS. Only one option allowed.",
        );
    }
    if !permissions_list.is_empty() {
        if no_source {
            return err(
                "given both NO_SOURCE_PERMISSIONS and FILE_PERMISSIONS. Only one option allowed.",
            );
        }
        if use_source {
            return err(
                "given both USE_SOURCE_PERMISSIONS and FILE_PERMISSIONS. Only one option allowed.",
            );
        }
    }
    if use_source && input_is_content {
        return err("given USE_SOURCE_PERMISSIONS without a file INPUT.");
    }
    let mut mode: u32 = 0;
    if no_source {
        mode |= 0o400 | 0o200 | 0o040 | 0o004;
    }
    let mut invalid = Vec::new();
    for name in &permissions_list {
        match permission_bit(name) {
            Some(bit) => mode |= bit,
            None => invalid.push(format!("\"{name}\"")),
        }
    }
    if !invalid.is_empty() {
        return err(format!("given invalid permission {}.", invalid.join(",")));
    }
    // AddEvaluationFile compiles OUTPUT and CONDITION here (the compiled
    // forms are evaluated at generate time without recompiling).
    drop(crate::profiling::entry("genex_compile", &output, || None));
    drop(crate::profiling::entry("genex_compile", &condition, || {
        None
    }));
    let dir = interp.mf_index();
    let backtrace = interp.backtrace.clone();
    let cmp0070 = interp.policy_status("CMP0070");
    interp.ext.evaluation_files.push(EvaluationFile {
        dir,
        input,
        input_is_content,
        output,
        condition,
        target,
        newline,
        permissions: mode,
        backtrace,
        cmp0070,
    });
    Ok(())
}

fn permission_bit(name: &str) -> Option<u32> {
    Some(match name {
        "OWNER_READ" => 0o400,
        "OWNER_WRITE" => 0o200,
        "OWNER_EXECUTE" => 0o100,
        "GROUP_READ" => 0o040,
        "GROUP_WRITE" => 0o020,
        "GROUP_EXECUTE" => 0o010,
        "WORLD_READ" => 0o004,
        "WORLD_WRITE" => 0o002,
        "WORLD_EXECUTE" => 0o001,
        "SETUID" => 0o4000,
        "SETGID" => 0o2000,
        _ => return None,
    })
}
