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
            interp.stderr_line(&text);
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
/// `cmMakefile::DisplayStatus` -> `cmake::UpdateProgress`: no progress
/// output while configuring a try_compile project.
/// `cmMakefile::DisplayStatus(text, -1)` through cmakemainProgressCallback:
/// with --debug-output a "Configuring..." status names the current source
/// directory, and the current makefile's call stack follows.
pub fn status_line(interp: &mut Interp, text: &str) {
    if interp.in_try_compile {
        return;
    }
    let mut line = format!("-- {text}");
    if interp.debug.output
        && !interp.ext.execution_file_bases.is_empty()
        && text.starts_with("Configuring")
    {
        line.push(' ');
        line.push_str(&interp.mf().source_dir);
    }
    line.push_str(&interp.debug_stack());
    if !interp.ext.no_progress {
        interp.stdout_line(&line);
    }
    // `cmMakefile::DisplayStatus` -> `OnMessageOutput(MESSAGE, message)`.
    interp.debug_message(crate::messages::MessageType::Message, text);
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

/// `WriteMessageEvent`: a `message-v1` event in CMakeConfigureLog.yaml.
fn configure_log(interp: &mut Interp, message: &str) {
    let Some(mut log) = crate::configure_log::get(interp) else {
        return;
    };
    if !log.is_any_log_version_enabled(&[1]) {
        return;
    }
    log.begin_event("message-v1", interp);
    log.write_literal_text_block("message", message);
    log.end_event();
}
