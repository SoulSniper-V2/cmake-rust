//! POSIX kwsys::Glob traversal and the file(GLOB) argument handling.

use std::fs;
use std::path::Path;

use crate::cmregex::Regex;
use crate::interp::{CmdResult, Interp, Mode, err};
use crate::messages::{Frame, MessageType};
use crate::util::{is_full_path, is_off, is_on};

use super::{fatal, source_path, sys_relative, system_error};

#[derive(Debug)]
pub struct GlobDependency {
    pub recurse: bool,
    pub list_directories: bool,
    pub follow_symlinks: bool,
    pub relative: String,
    pub expression: String,
    pub files: Vec<String>,
    pub invocations: Vec<(String, Vec<Frame>)>,
}

pub(super) fn pattern_regex(pattern: &str) -> String {
    let mut out = String::new();
    let chars: Vec<_> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' => out.push_str("[^/]*"),
            '?' => out.push_str("[^/]"),
            '[' => {
                let start = i + 1;
                let mut end = start;
                if matches!(chars.get(end), Some('!' | '^')) {
                    end += 1;
                }
                if chars.get(end) == Some(&']') {
                    end += 1;
                }
                while end < chars.len() && chars[end] != ']' {
                    end += 1;
                }
                if end == chars.len() {
                    out.push_str("\\[");
                } else {
                    out.push('[');
                    for (index, ch) in chars[start..end].iter().copied().enumerate() {
                        if ch == '!' && index == 0 {
                            out.push('^');
                        } else {
                            if ch == '\\' {
                                out.push('\\');
                            }
                            out.push(ch);
                        }
                    }
                    out.push(']');
                    i = end;
                }
            }
            ch => {
                if !ch.is_ascii_alphanumeric() {
                    out.push('\\');
                }
                out.push(ch);
            }
        }
        i += 1;
    }
    out
}

pub(super) enum GlobMessage {
    Cycle(String),
    Error(String),
}

struct Glob {
    patterns: Vec<Option<Regex>>,
    recurse: bool,
    list_dirs: bool,
    follow: bool,
    visited: Vec<String>,
    files: Vec<String>,
    messages: Vec<GlobMessage>,
}

impl Glob {
    fn walk(&mut self, base: &str, index: usize, recursive: bool) -> bool {
        let last = index + 1 == self.patterns.len();
        let recursive = recursive || last && self.recurse;
        let Ok(entries) = fs::read_dir(base) else {
            return true;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = if base.ends_with('/') && index == 0 {
                format!("{base}{name}")
            } else {
                format!("{base}/{name}")
            };
            let dir = Path::new(&path).is_dir();
            let link = entry.file_type().is_ok_and(|kind| kind.is_symlink());
            if recursive && dir && (!link || self.follow) {
                if link {
                    let canonical = match fs::canonicalize(base) {
                        Ok(path) => path.to_string_lossy().into_owned(),
                        Err(error) => {
                            self.messages.push(GlobMessage::Error(format!(
                                "Canonical path generation from path '{base}' failed! Reason: '{}'",
                                system_error(&error)
                            )));
                            return false;
                        }
                    };
                    if let Some(start) = self.visited.iter().position(|path| *path == canonical) {
                        let chain = self.visited[start..].join("\n");
                        self.messages
                            .push(GlobMessage::Cycle(format!("{chain}\n{canonical}/{name}")));
                        continue;
                    }
                    self.visited.push(canonical);
                }
                if self.list_dirs {
                    self.files.push(path.clone());
                }
                let success = self.walk(&path, index, true);
                if link {
                    self.visited.pop();
                }
                if !success {
                    return false;
                }
            } else if ((!recursive && !last && dir)
                || (last && (recursive || self.list_dirs || !dir)))
                && self.patterns[index]
                    .as_ref()
                    .is_some_and(|pattern| pattern.find(&name).is_some())
            {
                if last {
                    self.files.push(path);
                } else if !self.walk(&path, index + 1, false) {
                    return false;
                }
            }
        }
        true
    }
}

pub(super) fn walk(
    expression: &str,
    recurse: bool,
    list_dirs: bool,
    follow: bool,
) -> (Vec<String>, Vec<GlobMessage>) {
    let cwd = std::env::current_dir().unwrap_or_default();
    let expression = if is_full_path(expression) {
        expression.to_owned()
    } else {
        format!("{}/{expression}", cwd.display())
    };
    let mut skip = 0;
    let mut previous = b'\0';
    for (i, byte) in expression.bytes().enumerate() {
        if i > 0 && previous != b'\\' {
            match byte {
                b'/' => skip = i,
                b'[' | b'?' | b'*' => break,
                _ => {}
            }
        }
        previous = byte;
    }
    let base = format!("{}/", &expression[..skip]);
    let patterns: Vec<_> = expression[skip..]
        .split('/')
        .filter(|part| !part.is_empty())
        .map(|part| Regex::new(&format!("^{}$", pattern_regex(part))).ok())
        .collect();
    let mut glob = Glob {
        patterns,
        recurse,
        list_dirs,
        follow,
        visited: Vec::new(),
        files: Vec::new(),
        messages: Vec::new(),
    };
    if !glob.patterns.is_empty() {
        glob.walk(&base, 0, false);
    }
    (glob.files, glob.messages)
}

pub(super) fn run(interp: &mut Interp, args: &[String]) -> CmdResult {
    let action = &args[0];
    let Some(variable) = args.get(1) else {
        return err(format!(
            "{action} must be called with at least one additional argument."
        ));
    };
    let recurse = action == "GLOB_RECURSE";
    let mut list_dirs = !recurse;
    let mut follow = false;
    let mut relative = "";
    let mut tracked = false;
    let mut untracked = false;
    let mut results = Vec::new();
    let mut options = args[2..].iter().peekable();
    while let Some(arg) = options.next() {
        match arg.as_str() {
            "LIST_DIRECTORIES" => match options.next().map(String::as_str) {
                Some(value) if is_on(value) => list_dirs = true,
                Some(value) if is_off(value) => list_dirs = false,
                _ => return err("LIST_DIRECTORIES missing bool value."),
            },
            "FOLLOW_SYMLINKS" => {
                if recurse {
                    follow = true;
                    if options.peek().is_none() {
                        return err(
                            "GLOB_RECURSE requires a glob expression after FOLLOW_SYMLINKS.",
                        );
                    }
                }
            }
            "RELATIVE" => {
                let Some(value) = options.next() else {
                    return err("GLOB requires a directory after the RELATIVE tag.");
                };
                relative = value;
                if options.peek().is_none() {
                    return err("GLOB requires a glob expression after the directory.");
                }
            }
            "CONFIGURE_DEPENDS" => {
                if !tracked && untracked {
                    interp.issue(MessageType::AuthorWarning, "CONFIGURE_DEPENDS flag was given after a glob expression was already evaluated.");
                }
                if interp.mode != Mode::Project {
                    return Err(fatal(
                        interp,
                        "CONFIGURE_DEPENDS is invalid for script and find package modes.",
                    ));
                }
                tracked = true;
                if options.peek().is_none() {
                    return err("GLOB requires a glob expression after CONFIGURE_DEPENDS.");
                }
            }
            _ => {
                let expression = source_path(interp, arg);
                let (mut files, messages) = walk(&expression, recurse, list_dirs, follow);
                for message in messages {
                    match message {
                        GlobMessage::Cycle(path) => interp.issue(
                            MessageType::AuthorWarning,
                            &format!(
                                "Cyclic recursion detected while globbing for '{arg}':\n{path}"
                            ),
                        ),
                        GlobMessage::Error(error) => {
                            return Err(fatal(
                                interp,
                                &format!("Error has occurred while globbing for '{arg}' - {error}"),
                            ));
                        }
                    }
                }
                if !relative.is_empty() {
                    for file in &mut files {
                        *file = sys_relative(relative, file);
                    }
                }
                files.sort_unstable();
                files.dedup();
                results.extend(files.iter().cloned());
                if tracked {
                    let top = interp.top_source().map(str::to_owned);
                    let invocation = (variable.clone(), interp.backtrace.clone());
                    if let Some(previous) = interp.ext.glob_dependencies.iter_mut().find(|entry| {
                        entry.recurse == recurse
                            && entry.list_directories == list_dirs
                            && entry.follow_symlinks == follow
                            && entry.relative == relative
                            && entry.expression == expression
                    }) {
                        if previous.files != files {
                            let mut command = format!("file({action} {variable} ");
                            if follow {
                                command.push_str("FOLLOW_SYMLINKS ");
                            }
                            command.push_str(&format!("LIST_DIRECTORIES {list_dirs} "));
                            if !relative.is_empty() {
                                command.push_str(&format!("RELATIVE \"{relative}\" "));
                            }
                            command.push_str(&format!("\"{expression}\")"));
                            let mut message = format!(
                                "The glob expression\n {command}\nwas already present in the glob cache but the directory contents have changed during the configuration run.\nMatching glob expressions:"
                            );
                            for (variable, frames) in &previous.invocations {
                                message.push_str(&format!("\n  {variable}"));
                                if let Some(frame) = frames.last() {
                                    let file = top.as_deref().map_or_else(
                                        || frame.file.to_string(),
                                        |top| crate::util::relative_if_under(top, &frame.file),
                                    );
                                    if frame.line == 0 {
                                        message.push_str(&format!(" in {file}"));
                                    } else {
                                        message.push_str(&format!(" at {file}:{}", frame.line));
                                        if let Some(name) = &frame.name {
                                            message.push_str(&format!(" ({name})"));
                                        }
                                    }
                                }
                            }
                            return Err(fatal(interp, &message));
                        }
                        previous.invocations.push(invocation);
                    } else {
                        interp.ext.glob_dependencies.push(GlobDependency {
                            recurse,
                            list_directories: list_dirs,
                            follow_symlinks: follow,
                            relative: relative.into(),
                            expression,
                            files,
                            invocations: vec![invocation],
                        });
                    }
                } else {
                    untracked = true;
                }
            }
        }
    }
    results.sort_unstable();
    results.dedup();
    interp.set_def(variable, results.join(";"));
    Ok(())
}
