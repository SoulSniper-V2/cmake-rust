//! String-level helpers from cmGeneratorExpression.cxx.

use crate::util::expand_list;

use super::path::file_is_full_path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreprocessContext {
    Strip,
    BuildInterface,
    InstallInterface,
}

pub fn find(input: &str) -> Option<usize> {
    let open = input.find("$<")?;
    input[open + 2..].contains('>').then_some(open)
}

/// Whether the text contains an expression opener (cmGeneratorExpression::ContainsGenex).
pub fn contains_genex(input: &str) -> bool {
    find(input).is_some()
}

pub fn is_valid_target_name(input: &str) -> bool {
    !input.is_empty()
        && input
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.:+-".contains(&b))
}

pub fn strip_empty_list_elements(input: &str) -> String {
    input
        .split(';')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(";")
}

pub(super) fn balanced_end(input: &str, start: usize) -> Option<usize> {
    let mut depth = 1;
    let mut index = start + 2;
    let bytes = input.as_bytes();
    while index < bytes.len() {
        if bytes[index..].starts_with(b"$<") {
            depth += 1;
            index += 2;
            continue;
        }
        if bytes[index] == b'>' {
            depth -= 1;
            if depth == 0 {
                return Some(index + 1);
            }
        }
        index += 1;
    }
    None
}

pub fn strip(input: &str) -> String {
    let mut input = input;
    let mut result = String::new();
    let mut pending = 0usize;
    while let Some(start) = input.find("$<") {
        result.push_str(&input[..start]);
        let mut has_name = true;
        let bytes = input.as_bytes();
        let mut index = start + 2;
        loop {
            if index == bytes.len() {
                result.push_str(&input[start..]);
                return strip_empty_list_elements(&result);
            }
            if bytes[index..].starts_with(b"$<") {
                has_name = true;
                index += 2;
                continue;
            }
            match bytes[index] {
                b':' if has_name => {
                    pending += 1;
                    has_name = false;
                }
                b'>' => {
                    if !has_name && pending != 0 {
                        pending -= 1;
                    }
                    if pending == 0 {
                        break;
                    }
                }
                _ => {}
            }
            index += 1;
        }
        input = &input[index + 1..];
    }
    result.push_str(input);
    strip_empty_list_elements(&result)
}

pub fn preprocess(input: &str, context: PreprocessContext, import_prefix: &str) -> String {
    if context == PreprocessContext::Strip {
        return strip(input);
    }
    let mut input = input;
    let mut result = String::new();
    let markers = [
        "$<BUILD_INTERFACE:",
        "$<INSTALL_INTERFACE:",
        "$<BUILD_LOCAL_INTERFACE:",
    ];
    while let Some((start, marker)) = markers
        .iter()
        .filter_map(|marker| input.find(marker).map(|pos| (pos, *marker)))
        .min_by_key(|pair| pair.0)
    {
        result.push_str(&input[..start]);
        let Some(end) = balanced_end(input, start) else {
            input = &input[start..];
            break;
        };
        let content = &input[start + marker.len()..end - 1];
        match (marker, context) {
            ("$<BUILD_INTERFACE:", PreprocessContext::BuildInterface) => result.push_str(content),
            ("$<INSTALL_INTERFACE:", PreprocessContext::InstallInterface)
                if import_prefix.is_empty() =>
            {
                result.push_str(content)
            }
            ("$<INSTALL_INTERFACE:", PreprocessContext::InstallInterface) => {
                let mut items = split(content);
                for item in &mut items {
                    if !file_is_full_path(item) && find(item) != Some(0) {
                        item.insert_str(0, import_prefix);
                    }
                }
                result.push_str(&items.join(";"));
            }
            _ => {}
        }
        input = &input[end..];
    }
    result.push_str(input);
    strip_empty_list_elements(&result)
}

pub fn split(input: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut rest = input;
    while let Some(start) = rest.find("$<") {
        let item_start = rest[..start].rfind(';').map_or(0, |index| index + 1);
        result.extend(expand_list(&rest[..item_start], false));
        let end = balanced_end(rest, start).unwrap_or(rest.len());
        let item_end = rest[end..]
            .find(';')
            .map_or(rest.len(), |index| index + end);
        result.push(rest[item_start..item_end].to_owned());
        rest = &rest[item_end..];
    }
    result.extend(expand_list(rest, false));
    result
}

pub fn replace_install_prefix(input: &str, replacement: &str) -> String {
    let mut result = input.to_owned();
    let marker = "$<INSTALL_PREFIX>";
    let mut pos = 0;
    while let Some(offset) = result.get(pos..).and_then(|tail| tail.find(marker)) {
        let start = pos + offset;
        pos = start + marker.len();
        result.replace_range(start..pos, replacement);
    }
    result
}
