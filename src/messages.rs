//! Diagnostic formatting, mirroring `cmMessenger` and
//! `cmDocumentationFormatter`.

use std::rc::Rc;

use crate::util::relative_if_under;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageType {
    AuthorWarning,
    AuthorError,
    FatalError,
    InternalError,
    Message,
    Warning,
    Log,
    DeprecationError,
    DeprecationWarning,
}

/// One backtrace entry (`cmListFileContext`). `line == 0` marks a file
/// entry without a command.
#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub file: Rc<str>,
    pub line: usize,
    pub name: Option<Rc<str>>,
    /// Deferred-call placeholder marker.
    pub deferred: bool,
}

impl Frame {
    fn render(&self, top: Option<&str>) -> String {
        let file = match top {
            Some(top) => relative_if_under(top, &self.file),
            None => self.file.to_string(),
        };
        if self.line > 0 {
            match &self.name {
                Some(name) if !name.is_empty() => format!("{file}:{} ({name})", self.line),
                _ => format!("{file}:{}", self.line),
            }
        } else if self.deferred {
            format!("{file}:DEFERRED")
        } else {
            file
        }
    }
}

const TEXT_WIDTH: usize = 77;

fn format_line(out: &mut String, text: &str, padding: &str) {
    let tokens: Vec<&str> = text.split(' ').filter(|token| !token.is_empty()).collect();
    if tokens.is_empty() {
        return;
    }
    out.push_str(padding);
    let mut width = padding.len();
    let mut new_sentence = false;
    for token in tokens {
        let need_space = width > padding.len();
        let size = token.len() + usize::from(need_space) + usize::from(need_space && new_sentence);
        if width + size <= TEXT_WIDTH {
            if need_space {
                out.push_str(if new_sentence { "  " } else { " " });
            }
            out.push_str(token);
            width += size;
        } else {
            out.push('\n');
            out.push_str(padding);
            out.push_str(token);
            width = padding.len() + token.len();
        }
        new_sentence = token.ends_with('.');
    }
    out.push('\n');
}

/// `cmDocumentationFormatter::Format` with the given indent.
pub fn format_text(text: &str, indent: usize) -> String {
    if text.is_empty() {
        return String::new();
    }
    let padding = " ".repeat(indent);
    let mut out = String::new();
    let mut prev_preformatted = false;
    let mut start = 0usize;
    loop {
        let end = text[start..].find('\n').map(|offset| start + offset);
        let last = end.is_none();
        let line = match end {
            Some(end) => &text[start..end],
            None => &text[start..],
        };
        if line.starts_with(' ') {
            out.push_str(&padding);
            out.push_str(line);
            prev_preformatted = true;
        } else {
            if prev_preformatted {
                out.push('\n');
            }
            if line.is_empty() {
                if !last {
                    out.push('\n');
                }
            } else {
                format_line(&mut out, line, &padding);
            }
            prev_preformatted = false;
        }
        if !last {
            out.push('\n');
        }
        match end {
            Some(end) if end + 1 < text.len() => start = end + 1,
            Some(_) => break,
            None => break,
        }
    }
    if prev_preformatted {
        out.push('\n');
    }
    out
}

fn type_str(kind: MessageType) -> &'static str {
    match kind {
        MessageType::FatalError => "Error",
        MessageType::InternalError => "Internal Error (please report a bug)",
        MessageType::Log => "Debug Log",
        MessageType::DeprecationError => "Deprecation Error",
        MessageType::DeprecationWarning => "Deprecation Warning",
        MessageType::AuthorWarning => "Warning (dev)",
        MessageType::AuthorError => "Error (dev)",
        _ => "Warning",
    }
}

pub fn is_error(kind: MessageType) -> bool {
    matches!(
        kind,
        MessageType::FatalError
            | MessageType::InternalError
            | MessageType::DeprecationError
            | MessageType::AuthorError
    )
}

/// Renders a full diagnostic like `cmMessenger::DisplayMessage`.
pub fn render(kind: MessageType, text: &str, backtrace: &[Frame], top: Option<&str>) -> String {
    let mut msg = format!("CMake {}", type_str(kind));
    if let Some(frame) = backtrace.last() {
        msg.push_str(if frame.line > 0 { " at " } else { " in " });
        msg.push_str(&frame.render(top));
    }
    msg.push_str(":\n");
    msg.push_str(&format_text(text, 2));
    // Call stack.
    if backtrace.len() >= 2 {
        let mut last_file = backtrace.last().unwrap().file.clone();
        let mut first = true;
        for frame in backtrace[..backtrace.len() - 1].iter().rev() {
            if frame.name.as_deref().is_none_or(str::is_empty)
                && !frame.deferred
                && frame.file == last_file
            {
                continue;
            }
            if first {
                first = false;
                msg.push_str("Call Stack (most recent call first):\n");
            }
            last_file = frame.file.clone();
            msg.push_str("  ");
            msg.push_str(&frame.render(top));
            msg.push('\n');
        }
    }
    match kind {
        MessageType::AuthorWarning => {
            msg.push_str("This warning is for project developers.  Use -Wno-dev to suppress it.")
        }
        MessageType::AuthorError => {
            msg.push_str("This error is for project developers. Use -Wno-error=dev to suppress it.")
        }
        _ => {}
    }
    msg.push('\n');
    msg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps() {
        let text = "aaaa. bbbb";
        assert_eq!(format_text(text, 2), "  aaaa.  bbbb\n");
        assert_eq!(format_text("a\nb", 2), "  a\n\n  b\n");
        assert_eq!(format_text("x\n pre\ny", 2), "  x\n\n   pre\n\n  y\n");
    }
}
