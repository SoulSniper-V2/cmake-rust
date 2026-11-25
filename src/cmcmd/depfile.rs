//! GCC depfile lexer and cmTransformDepfile output formats.

use std::fs;
use std::path::Path;

use crate::{messages, util};

#[derive(Default)]
pub(crate) struct Rule {
    pub(crate) targets: Vec<String>,
    pub(crate) dependencies: Vec<String>,
}

pub(super) fn transform(args: &[String]) -> i32 {
    let [
        generator,
        format,
        source,
        _,
        binary,
        current_binary,
        input,
        output,
    ] = args
    else {
        return 1;
    };
    if !matches!(
        format.as_str(),
        "gccdepfile" | "makedepfile" | "MSBuildAdditionalInputs"
    ) || !matches!(
        generator.as_str(),
        "Ninja" | "Ninja Multi-Config" | "Unix Makefiles" | "Xcode"
    ) {
        return 1;
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    let cwd = cwd.to_string_lossy();
    let normalize = |path| util::collapse_full_path(path, &cwd);
    let source = normalize(source);
    let binary = normalize(binary);
    let current_binary = normalize(current_binary);
    let rules = match fs::read(input) {
        Ok(content) => {
            let Some(rules) = parse(&String::from_utf8_lossy(&content)) else {
                return 2;
            };
            rules
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!(
                "{}",
                messages::render(
                    messages::MessageType::Warning,
                    &format!("Expected depfile does not exist.\n  {input}"),
                    &[],
                    None,
                )
            );
            Vec::new()
        }
        Err(_) => return 2,
    };
    // cmReadGccDepfile: a relative path gets the prefix and "/", and only a
    // full path is collapsed (so "a:b.h" is never taken for a drive root).
    let absolute = |path: &str| {
        let path = crate::cache::convert_to_unix_slashes(path);
        let path = if util::is_full_path(&path) {
            path
        } else {
            format!("{current_binary}/{path}")
        };
        if util::is_full_path(&path) {
            util::collapse_full_path(&path, "")
        } else {
            path
        }
    };
    let format_gcc = |path: &str| {
        let path = absolute(path);
        let in_binary = path == binary || util::is_subdirectory(&path, &binary);
        let source_in_binary = util::is_subdirectory(&source, &binary);
        let in_nested_source =
            source_in_binary && (path == source || util::is_subdirectory(&path, &source));
        let path = if generator != "Xcode" && in_binary && !in_nested_source {
            util::relative_if_under(&binary, &path)
        } else {
            path
        };
        path.replace('\\', "\\\\").replace(' ', "\\ ")
    };
    let mut text = String::new();
    if format == "MSBuildAdditionalInputs" {
        if !rules.is_empty() {
            text.push('\u{feff}');
            let dependencies = rules
                .iter()
                .flat_map(|rule| rule.dependencies.iter())
                .map(|path| absolute(path).replace('/', "\\"))
                .collect::<Vec<_>>();
            text.push_str(&dependencies.join(";"));
            text.push('\n');
        }
    } else {
        let separator = " \\\n  ";
        for rule in &rules {
            let targets = rule
                .targets
                .iter()
                .map(|path| format_gcc(path))
                .collect::<Vec<_>>();
            text.push_str(&targets.join(separator));
            text.push(':');
            for path in &rule.dependencies {
                text.push_str(separator);
                text.push_str(&format_gcc(path));
            }
            text.push('\n');
        }
        if format == "makedepfile" {
            text.push('\n');
            for path in rules.iter().flat_map(|rule| &rule.dependencies) {
                text.push('\n');
                text.push_str(&format_gcc(path));
                text.push_str(":\n");
            }
        }
    }
    if let Some(parent) = Path::new(output).parent() {
        let _ = fs::create_dir_all(parent);
    }
    if fs::write(output, text).is_ok() {
        0
    } else {
        2
    }
}

pub(crate) fn parse(text: &str) -> Option<Vec<Rule>> {
    let mut rules = Vec::new();
    let mut rule = Rule::default();
    let mut token = String::new();
    let mut dependencies = false;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                let mut count = 1;
                while chars.next_if_eq(&'\\').is_some() {
                    count += 1;
                }
                if chars.peek() == Some(&' ') {
                    chars.next();
                    if count % 2 == 0 {
                        token.extend(std::iter::repeat_n('\\', count));
                        flush_token(&mut rule, &mut token, dependencies);
                        dependencies = true;
                    } else {
                        token.extend(std::iter::repeat_n('\\', count / 2));
                        token.push(' ');
                    }
                } else {
                    token.extend(std::iter::repeat_n('\\', count - 1));
                    match chars.peek().copied() {
                        Some('#' | ':') => token.push(chars.next()?),
                        Some('\n' | '\r') => {
                            let next = chars.next()?;
                            if next == '\r' && chars.next_if_eq(&'\n').is_none() {
                                token.push('\\');
                                token.push('\r');
                            } else {
                                flush_token(&mut rule, &mut token, dependencies);
                            }
                        }
                        _ => token.push('\\'),
                    }
                }
            }
            '$' => {
                chars.next_if_eq(&'$');
                token.push('$');
            }
            ':' if matches!(chars.peek(), Some(' ' | '\t' | '\n' | '\r'))
                || matches!(
                    chars.clone().take(3).collect::<String>().as_str(),
                    "\\\n" | "\\\r\n"
                )
                || chars.clone().take(2).eq(['\\', '\n']) =>
            {
                flush_token(&mut rule, &mut token, dependencies);
                dependencies = true;
            }
            ' ' | '\t' => flush_token(&mut rule, &mut token, dependencies),
            '\n' | '\r' if ch == '\n' || chars.next_if_eq(&'\n').is_some() => {
                flush_token(&mut rule, &mut token, dependencies);
                if !dependencies && !rule.targets.is_empty() {
                    return None;
                }
                finish_rule(&mut rules, std::mem::take(&mut rule));
                dependencies = false;
            }
            _ => token.push(ch),
        }
    }
    flush_token(&mut rule, &mut token, dependencies);
    finish_rule(&mut rules, rule);
    Some(rules)
}

fn flush_token(rule: &mut Rule, token: &mut String, dependencies: bool) {
    if !token.is_empty() {
        let list = if dependencies {
            &mut rule.dependencies
        } else {
            &mut rule.targets
        };
        list.push(std::mem::take(token));
    }
}

fn finish_rule(rules: &mut Vec<Rule>, mut rule: Rule) {
    if !rule.targets.is_empty() {
        rule.dependencies.sort();
        rule.dependencies.dedup();
        rules.push(rule);
    }
}
