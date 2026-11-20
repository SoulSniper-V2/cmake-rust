//! cmCMakePath lexical operations. In particular, "." and trailing path
//! separators participate in comparison, unlike std::path::Path equality.

use super::{boolean, cm_list, list::check_count};

fn separator(c: char) -> bool {
    c == '/' || (cfg!(windows) && c == '\\')
}

#[derive(Clone, Copy)]
struct LexicalPath<'a> {
    raw: &'a str,
    root_name: &'a str,
    root_dir: &'a str,
    relative: &'a str,
}

impl<'a> LexicalPath<'a> {
    fn new(raw: &'a str) -> Self {
        let mut root_name = "";
        if cfg!(windows) {
            let bytes = raw.as_bytes();
            if bytes.first().is_some_and(u8::is_ascii_alphabetic) && bytes.get(1) == Some(&b':') {
                root_name = &raw[..2];
            } else if raw.starts_with(['/', '\\'])
                && raw
                    .get(1..)
                    .is_some_and(|tail| tail.starts_with(['/', '\\']))
                && raw
                    .get(2..)
                    .is_some_and(|tail| !tail.is_empty() && !tail.starts_with(separator))
            {
                let end = raw[2..].find(separator).map_or(raw.len(), |n| n + 2);
                root_name = &raw[..end];
            }
        }
        let rest = &raw[root_name.len()..];
        let relative = rest.trim_start_matches(separator);
        let root_dir = &rest[..rest.len() - relative.len()];
        Self {
            raw,
            root_name,
            root_dir,
            relative,
        }
    }

    fn components(self) -> Vec<&'a str> {
        let mut parts: Vec<_> = self
            .relative
            .split(separator)
            .filter(|part| !part.is_empty())
            .collect();
        if !parts.is_empty() && self.relative.ends_with(separator) {
            parts.push("");
        }
        parts
    }

    fn absolute(self) -> bool {
        !self.root_dir.is_empty() && (!cfg!(windows) || !self.root_name.is_empty())
    }

    fn filename(self) -> &'a str {
        self.relative.rsplit(separator).next().unwrap_or("")
    }

    fn parent(self) -> &'a str {
        if self.relative.is_empty() {
            return self.raw;
        }
        let root_len = self.raw.len() - self.relative.len();
        let offset = self.relative.len() - self.filename().len();
        &self.raw[..root_len + self.relative[..offset].trim_end_matches(separator).len()]
    }

    fn without_filename(self) -> &'a str {
        &self.raw[..self.raw.len() - self.filename().len()]
    }

    fn same_root(self, rhs: Self) -> bool {
        generic(self.root_name) == generic(rhs.root_name)
            && self.root_dir.is_empty() == rhs.root_dir.is_empty()
    }

    fn extension(self, last_only: bool) -> &'a str {
        let file = self.filename();
        if matches!(file, "" | "." | "..") {
            return "";
        }
        let offset = usize::from(file.starts_with('.'));
        let base = &file[offset..];
        let dot = if last_only {
            base.rfind('.')
        } else {
            base.find('.')
        };
        dot.map_or("", |dot| &file[dot + offset..])
    }

    fn normal(self) -> String {
        if self.raw.is_empty() {
            return String::new();
        }
        let parts = self.components();
        let mut output = Vec::new();
        let mut trailing = false;
        for part in parts {
            trailing = false;
            match part {
                "" | "." => trailing = true,
                ".." if output.last().is_some_and(|v| *v != "..") => {
                    output.pop();
                    trailing = true;
                }
                ".." if !self.root_dir.is_empty() => {}
                _ => output.push(part),
            }
        }
        let mut result = self.generic_root();
        result.push_str(&output.join("/"));
        if trailing && output.last().is_some_and(|part| *part != "..") {
            result.push('/');
        }
        if result.is_empty() {
            result.push('.');
        }
        result
    }

    fn generic_root(self) -> String {
        let mut result = generic(self.root_name);
        if !self.root_dir.is_empty() {
            result.push('/');
        }
        result
    }

    fn relative_to(self, base: Self) -> String {
        if !self.same_root(base) || self.absolute() != base.absolute() {
            return String::new();
        }
        let lhs = self.components();
        let rhs = base.components();
        let common = lhs.iter().zip(&rhs).take_while(|(a, b)| a == b).count();
        let base_up = rhs[common..]
            .iter()
            .map(|item| match *item {
                ".." => -1isize,
                "." | "" => 0,
                _ => 1,
            })
            .sum::<isize>();
        if base_up < 0 {
            return String::new();
        }
        let mut parts = vec![".."; base_up as usize];
        parts.extend_from_slice(&lhs[common..]);
        let result = parts.join("/");
        if result.is_empty() {
            ".".to_owned()
        } else {
            result
        }
    }
}

fn generic(raw: &str) -> String {
    let mut result = String::with_capacity(raw.len());
    let mut previous = false;
    for c in raw.chars() {
        let slash = separator(c);
        if !slash || !previous {
            result.push(if slash { '/' } else { c });
        }
        previous = slash;
    }
    if cfg!(windows) && raw.starts_with("//") && raw.len() > 2 && !raw[2..].starts_with(separator) {
        result.insert(0, '/');
    }
    result
}

pub(super) fn equal(lhs: &str, rhs: &str) -> bool {
    let lhs = LexicalPath::new(lhs);
    let rhs = LexicalPath::new(rhs);
    lhs.same_root(rhs) && lhs.components() == rhs.components()
}

fn prefix(lhs: &str, rhs: &str) -> bool {
    let lhs = LexicalPath::new(lhs);
    let rhs = LexicalPath::new(rhs);
    if !lhs.same_root(rhs) {
        return lhs.raw.is_empty();
    }
    let mut a = lhs.components();
    let b = rhs.components();
    if a.last() == Some(&"") && b.len() >= a.len() {
        a.pop();
    }
    b.starts_with(&a)
}

fn append(lhs: &str, rhs: &str) -> String {
    let left = LexicalPath::new(lhs);
    let right = LexicalPath::new(rhs);
    if right.absolute() || (!right.root_name.is_empty() && right.root_name != left.root_name) {
        return rhs.to_owned();
    }
    if !right.root_dir.is_empty() {
        return format!("{}{}", left.root_name, &rhs[right.root_name.len()..]);
    }
    let mut output = lhs.to_owned();
    if !left.filename().is_empty() || (left.absolute() && left.root_dir.is_empty()) {
        output.push(if cfg!(windows) { '\\' } else { '/' });
    }
    output.push_str(right.relative);
    output
}

pub(super) fn evaluate(args: &[String]) -> Result<String, String> {
    let [op, rest @ ..] = args else {
        unreachable!()
    };
    let option = match op.as_str() {
        "GET_EXTENSION" | "GET_STEM" | "REMOVE_EXTENSION" | "REPLACE_EXTENSION" => {
            Some("LAST_ONLY")
        }
        "IS_PREFIX" | "CMAKE_PATH" | "NATIVE_PATH" | "ABSOLUTE_PATH" => Some("NORMALIZE"),
        _ => None,
    };
    let modifier = option.filter(|value| *value == rest[0]);
    let args = &rest[usize::from(modifier.is_some())..];
    let (required, exact) = match op.as_str() {
        "GET_ROOT_NAME" | "GET_ROOT_DIRECTORY" | "GET_ROOT_PATH" | "GET_FILENAME"
        | "GET_EXTENSION" | "GET_STEM" | "GET_RELATIVE_PART" | "GET_PARENT_PATH"
        | "HAS_ROOT_NAME" | "HAS_ROOT_DIRECTORY" | "HAS_ROOT_PATH" | "HAS_FILENAME"
        | "HAS_EXTENSION" | "HAS_STEM" | "HAS_RELATIVE_PART" | "HAS_PARENT_PATH"
        | "IS_ABSOLUTE" | "IS_RELATIVE" | "CMAKE_PATH" | "NATIVE_PATH" | "REMOVE_FILENAME"
        | "REMOVE_EXTENSION" | "NORMAL_PATH" => (1, true),
        "APPEND" => (1, false),
        "IS_PREFIX" | "REPLACE_FILENAME" | "REPLACE_EXTENSION" | "RELATIVE_PATH"
        | "ABSOLUTE_PATH" => (2, true),
        _ => return Err(format!("{op}: invalid option.")),
    };
    let label = modifier.map_or_else(|| op.clone(), |modifier| format!("{op},{modifier}"));
    check_count("PATH", &label, args.len(), required, exact)?;
    let path = LexicalPath::new(&args[0]);
    let normalize = modifier == Some("NORMALIZE");
    let last_only = modifier == Some("LAST_ONLY");
    let predicate = match op.as_str() {
        "HAS_ROOT_NAME" => !path.root_name.is_empty(),
        "HAS_ROOT_DIRECTORY" => !path.root_dir.is_empty(),
        "HAS_ROOT_PATH" => !path.root_name.is_empty() || !path.root_dir.is_empty(),
        "HAS_FILENAME" => !path.filename().is_empty(),
        "HAS_EXTENSION" => !path.extension(true).is_empty(),
        "HAS_STEM" => !path.filename().is_empty(),
        "HAS_RELATIVE_PART" => !path.relative.is_empty(),
        "HAS_PARENT_PATH" => !path.parent().is_empty(),
        "IS_ABSOLUTE" => path.absolute(),
        "IS_RELATIVE" => !path.absolute(),
        "IS_PREFIX" => {
            return Ok(boolean(if normalize {
                prefix(&path.normal(), &LexicalPath::new(&args[1]).normal())
            } else {
                prefix(path.raw, &args[1])
            }));
        }
        _ => {
            let values: Vec<_> = cm_list(path.raw, false)
                .iter()
                .map(|value| {
                    let path = LexicalPath::new(value);
                    let file = path.filename();
                    let ext = path.extension(last_only);
                    let stem = &file[..file.len() - ext.len()];
                    match op.as_str() {
                        "GET_ROOT_NAME" => path.root_name.to_owned(),
                        "GET_ROOT_DIRECTORY" => path
                            .root_dir
                            .chars()
                            .next()
                            .map_or(String::new(), |c| c.to_string()),
                        "GET_ROOT_PATH" => format!(
                            "{}{}",
                            path.root_name,
                            path.root_dir
                                .chars()
                                .next()
                                .map_or(String::new(), |c| c.to_string())
                        ),
                        "GET_FILENAME" => file.to_owned(),
                        "GET_EXTENSION" => ext.to_owned(),
                        "GET_STEM" => stem.to_owned(),
                        "GET_RELATIVE_PART" => path.relative.to_owned(),
                        "GET_PARENT_PATH" => path.parent().to_owned(),
                        "CMAKE_PATH" => {
                            let mut raw = path.raw;
                            if cfg!(windows) {
                                raw = raw
                                    .strip_prefix("//?/")
                                    .or_else(|| raw.strip_prefix("\\\\?\\"))
                                    .unwrap_or(raw);
                                if raw.starts_with("UNC/") || raw.starts_with("UNC\\") {
                                    let converted = format!("//{}", &raw[4..]);
                                    return if normalize {
                                        LexicalPath::new(&converted).normal()
                                    } else {
                                        generic(&converted)
                                    };
                                }
                            }
                            if normalize {
                                LexicalPath::new(raw).normal()
                            } else {
                                generic(raw)
                            }
                        }
                        "NATIVE_PATH" => {
                            let path = if normalize {
                                path.normal()
                            } else {
                                value.clone()
                            };
                            if cfg!(windows) {
                                path.replace('/', "\\")
                            } else {
                                path
                            }
                        }
                        "APPEND" => args[1..]
                            .iter()
                            .fold(value.clone(), |base, arg| append(&base, arg)),
                        "REMOVE_FILENAME" => path.without_filename().to_owned(),
                        "REPLACE_FILENAME" if !file.is_empty() => {
                            append(path.without_filename(), &args[1])
                        }
                        "REPLACE_FILENAME" => value.clone(),
                        "REMOVE_EXTENSION" => value[..value.len() - ext.len()].to_owned(),
                        "REPLACE_EXTENSION" => {
                            let replacement = &args[1];
                            let dot = if replacement.is_empty() || replacement.starts_with('.') {
                                ""
                            } else {
                                "."
                            };
                            format!("{}{dot}{replacement}", &value[..value.len() - ext.len()])
                        }
                        "NORMAL_PATH" => path.normal(),
                        "RELATIVE_PATH" => path.relative_to(LexicalPath::new(&args[1])),
                        "ABSOLUTE_PATH" => {
                            let absolute = if path.absolute() {
                                value.clone()
                            } else {
                                generic(&append(&args[1], value))
                            };
                            if normalize {
                                LexicalPath::new(&absolute).normal()
                            } else {
                                absolute
                            }
                        }
                        _ => unreachable!(),
                    }
                })
                .collect();
            return Ok(values.join(";"));
        }
    };
    Ok(boolean(predicate))
}
