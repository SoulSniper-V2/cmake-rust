//! Message levels, check reporting, and configure log events.

use crate::interp::{CmdResult, Interp, err};
use crate::messages::MessageType;
use crate::util;

pub fn register(interp: &mut Interp) {
    interp.register("message", message);
}

fn indent_text(interp: &Interp, text: &str) -> String {
    let mut indent = interp
        .get_def("CMAKE_MESSAGE_INDENT")
        .map(|value| util::expand_list(value, false).concat())
        .unwrap_or_default();
    if (interp
        .ext
        .misc
        .get("log-context")
        .is_some_and(|value| value == "1")
        || interp.is_on("CMAKE_MESSAGE_CONTEXT_SHOW"))
        && let Some(context) = interp.get_def("CMAKE_MESSAGE_CONTEXT")
    {
        let context = util::expand_list(context, false).join(".");
        if !context.is_empty() {
            indent.insert_str(0, &format!("[{context}] "));
        }
    }
    if indent.is_empty() {
        return text.to_owned();
    }
    let mut out = String::new();
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(&indent);
        out.push_str(line);
    }
    out
}

/// Log levels (`Message::LogLevel`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum LogLevel {
    Error = 1,
    Warning,
    Notice,
    Status,
    Verbose,
    Debug,
    Trace,
}

pub fn parse_log_level(text: &str) -> Option<LogLevel> {
    Some(match text.to_ascii_uppercase().as_str() {
        "ERROR" => LogLevel::Error,
        "WARNING" => LogLevel::Warning,
        "NOTICE" => LogLevel::Notice,
        "STATUS" => LogLevel::Status,
        "VERBOSE" => LogLevel::Verbose,
        "DEBUG" => LogLevel::Debug,
        "TRACE" => LogLevel::Trace,
        _ => return None,
    })
}

/// `cmMakefile::GetCurrentLogLevel`: command-line level wins, then
/// CMAKE_MESSAGE_LOG_LEVEL.
pub fn current_log_level(interp: &Interp) -> LogLevel {
    if let Some(level) = interp
        .ext
        .misc
        .get("log-level")
        .and_then(|level| parse_log_level(level))
    {
        return level;
    }
    interp
        .get_def("CMAKE_MESSAGE_LOG_LEVEL")
        .and_then(parse_log_level)
        .unwrap_or(LogLevel::Status)
}

fn message(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.is_empty() {
        return err("called with incorrect number of arguments");
    }
    let mut index = 1;
    let mut kind = MessageType::Message;
    let mut fatal = false;
    let mut check: Option<&str> = None;
    let level = match args[0].as_str() {
        "SEND_ERROR" => {
            kind = MessageType::FatalError;
            LogLevel::Error
        }
        "FATAL_ERROR" => {
            fatal = true;
            kind = MessageType::FatalError;
            LogLevel::Error
        }
        "WARNING" => {
            kind = MessageType::Warning;
            LogLevel::Warning
        }
        "AUTHOR_WARNING" => {
            if interp.is_set("CMAKE_SUPPRESS_DEVELOPER_ERRORS")
                && !interp.is_on("CMAKE_SUPPRESS_DEVELOPER_ERRORS")
            {
                fatal = true;
                kind = MessageType::AuthorError;
                LogLevel::Error
            } else if !interp.is_on("CMAKE_SUPPRESS_DEVELOPER_WARNINGS") {
                kind = MessageType::AuthorWarning;
                LogLevel::Warning
            } else {
                return Ok(());
            }
        }
        "CHECK_START" | "CHECK_PASS" | "CHECK_FAIL" => {
            check = Some(match args[0].as_str() {
                "CHECK_START" => "start",
                "CHECK_PASS" => "pass",
                _ => "fail",
            });
            LogLevel::Status
        }
        "CONFIGURE_LOG" => {
            configure_log(interp, &args[1..].concat());
            return Ok(());
        }
        "STATUS" => LogLevel::Status,
        "VERBOSE" => LogLevel::Verbose,
        "DEBUG" => LogLevel::Debug,
        "TRACE" => LogLevel::Trace,
        "DEPRECATION" => {
            if interp.is_on("CMAKE_ERROR_DEPRECATED") {
                fatal = true;
                kind = MessageType::DeprecationError;
                LogLevel::Error
            } else if !interp.is_set("CMAKE_WARN_DEPRECATED")
                || interp.is_on("CMAKE_WARN_DEPRECATED")
            {
                kind = MessageType::DeprecationWarning;
                LogLevel::Warning
            } else {
                return Ok(());
            }
        }
        "NOTICE" => LogLevel::Notice,
        _ => {
            index = 0;
            LogLevel::Notice
        }
    };
    if current_log_level(interp) < level {
        return Ok(());
    }
    let text = args[index..].concat();
    match level {
        LogLevel::Error | LogLevel::Warning => {
            interp.issue_at_unsuppressed(kind, &text);
        }
        LogLevel::Notice => {
            let text = indent_text(interp, &text);
            eprintln!("{text}");
        }
        LogLevel::Status => match check {
            Some("start") => {
                let shown = indent_text(interp, &text);
                status_line(interp, &shown);
                interp.ext.message_checks.push(text);
            }
            Some(_) => {
                let started = interp.ext.message_checks.pop();
                match started {
                    Some(started) => {
                        let shown = indent_text(interp, &format!("{started} - {text}"));
                        status_line(interp, &shown);
                    }
                    None => {
                        let text = format!(
                            "Ignored {} without CHECK_START",
                            if check == Some("pass") {
                                "CHECK_PASS"
                            } else {
                                "CHECK_FAIL"
                            }
                        );
                        interp.issue_at_unsuppressed(MessageType::AuthorWarning, &text);
                    }
                }
            }
            None => {
                let shown = indent_text(interp, &text);
                status_line(interp, &shown);
            }
        },
        _ => {
            let shown = indent_text(interp, &text);
            status_line(interp, &shown);
        }
    }
    if fatal {
        interp.set_fatal();
    }
    Ok(())
}

/// `cmMakefile::DisplayStatus` writes one status prefix per message.
pub fn status_line(interp: &mut Interp, text: &str) {
    interp.stdout_line(&format!("-- {text}"));
}

impl LogLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "ERROR",
            Self::Warning => "WARNING",
            Self::Notice => "NOTICE",
            Self::Status => "STATUS",
            Self::Verbose => "VERBOSE",
            Self::Debug => "DEBUG",
            Self::Trace => "TRACE",
        }
    }
}

/// A configure run writes one YAML document even when it logs many events.
#[derive(Debug)]
pub struct ConfigureLog(std::fs::File);

impl Drop for ConfigureLog {
    fn drop(&mut self) {
        use std::io::Write;
        let _ = writeln!(self.0, "...");
    }
}

fn configure_log(interp: &mut Interp, message: &str) {
    use std::io::Write;

    if interp.mode == crate::interp::Mode::Script {
        return;
    }
    let result = (|| -> std::io::Result<()> {
        if interp.ext.configure_log.is_none() {
            let dir = std::path::Path::new(&interp.home_binary).join("CMakeFiles");
            std::fs::create_dir_all(&dir)?;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("CMakeConfigureLog.yaml"))?;
            writeln!(file, "\n---\nevents:")?;
            interp.ext.configure_log = Some(ConfigureLog(file));
        }
        let log = &mut interp
            .ext
            .configure_log
            .as_mut()
            .expect("open configure log")
            .0;
        writeln!(log, "  -\n    kind: \"message-v1\"\n    backtrace:")?;
        for frame in interp
            .backtrace
            .iter()
            .rev()
            .filter(|frame| frame.name.is_some() || frame.deferred)
        {
            let path = frame
                .file
                .strip_prefix(&interp.home_source)
                .and_then(|relative| relative.strip_prefix('/'))
                .unwrap_or(&frame.file);
            let entry = if frame.deferred {
                format!("{path}:DEFERRED")
            } else {
                format!(
                    "{path}:{} ({})",
                    frame.line,
                    frame.name.as_deref().unwrap_or("")
                )
            };
            writeln!(log, "      - {}", serde_json::Value::String(entry))?;
        }
        if !interp.ext.message_checks.is_empty() {
            writeln!(log, "    checks:")?;
            for check in interp.ext.message_checks.iter().rev() {
                writeln!(log, "      - {}", serde_json::Value::String(check.clone()))?;
            }
        }
        writeln!(log, "    message: |")?;
        let mut lines = message.split_inclusive('\n');
        for line in &mut lines {
            write!(log, "      ")?;
            let line = line
                .strip_suffix('\n')
                .map_or(line, |line| line.strip_suffix('\r').unwrap_or(line));
            for ch in line.chars() {
                match ch {
                    '\\' => write!(log, "\\\\")?,
                    '\t' => write!(log, "\t")?,
                    ch if ch.is_control() => {
                        for byte in ch.encode_utf8(&mut [0; 4]).bytes() {
                            write!(log, "\\x{byte:02x}")?;
                        }
                    }
                    _ => write!(log, "{ch}")?,
                }
            }
            writeln!(log)?;
        }
        Ok(())
    })();
    let _ = result;
}
