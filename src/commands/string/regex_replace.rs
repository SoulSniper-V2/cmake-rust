//! Port of `cmStringReplaceHelper` (REGEX REPLACE `\N` back-references)
//! and the `RegexReplace` handler from `cmStringCommand.cxx`.

use crate::interp::{CmdResult, Interp, err};
use crate::state::{clear_matches, store_matches};

use super::{cm_join, old_bol, regex_find_next};

pub enum Segment {
    Literal(String),
    Group(usize),
}

/// `cmStringReplaceHelper::ParseReplaceExpression`, over CMake's raw bytes:
/// an unknown escape names the single byte after the backslash, even when it
/// starts a multi-byte character. Returns the error text on failure.
pub fn parse_replace_expression(expr: &str) -> Result<Vec<Segment>, String> {
    let raw = crate::bytes::encode(expr);
    let bytes: &[u8] = &raw;
    let mut replacements = Vec::new();
    let mut l = 0usize;
    while l < bytes.len() {
        let Some(r) = bytes[l..].iter().position(|&b| b == b'\\').map(|p| l + p) else {
            replacements.push(Segment::Literal(crate::bytes::decode(&bytes[l..])));
            break;
        };
        if r > l {
            replacements.push(Segment::Literal(crate::bytes::decode(&bytes[l..r])));
        }
        if r == bytes.len() - 1 {
            return Err("replace-expression ends in a backslash".to_owned());
        }
        let next = bytes[r + 1];
        if next.is_ascii_digit() {
            replacements.push(Segment::Group(usize::from(next - b'0')));
        } else if next == b'n' {
            replacements.push(Segment::Literal("\n".to_owned()));
        } else if next == b'\\' {
            replacements.push(Segment::Literal("\\".to_owned()));
        } else {
            let mut text = b"Unknown escape \"".to_vec();
            text.extend_from_slice(&bytes[r..r + 2]);
            text.extend_from_slice(b"\" in replace-expression");
            return Err(crate::bytes::decode(&text));
        }
        l = r + 2;
    }
    Ok(replacements)
}

/// Counts capturing groups the way kwsys `regnpar` does: every unescaped `(`
/// outside a bracket expression (brackets have no backslash handling and an
/// immediate `]` or `-` is literal).
pub fn count_groups(pattern: &str) -> usize {
    let bytes = pattern.as_bytes();
    let mut groups = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                i += 1;
            }
            b'[' => {
                i += 1;
                if i < bytes.len() && bytes[i] == b'^' {
                    i += 1;
                }
                if i < bytes.len() && matches!(bytes[i], b']' | b'-') {
                    i += 1;
                }
                while i < bytes.len() && bytes[i] != b']' {
                    i += 1;
                }
            }
            b'(' => groups += 1,
            _ => {}
        }
        i += 1;
    }
    groups
}

/// `RegexReplace` (`cmStringReplaceHelper::Replace`).
pub fn regex_replace(interp: &mut Interp, args: &[String]) -> CmdResult {
    let regex = &args[2];
    let replace = &args[3];
    let outvar = &args[4];

    let replacements = match parse_replace_expression(replace) {
        Ok(replacements) => replacements,
        Err(error) => {
            return err(format!("sub-command REGEX, mode REPLACE: {error}."));
        }
    };

    clear_matches(interp);

    let re = match crate::cmregex::Regex::new(regex) {
        Ok(re) => re,
        Err(_) => {
            return err(format!(
                "sub-command REGEX, mode REPLACE failed to compile regex \"{regex}\"."
            ));
        }
    };
    let num_groups = count_groups(regex);

    let input = cm_join(&args[5..], "", "");
    let old_policy = old_bol(interp);

    let input_bytes = crate::bytes::encode(&input);
    // Byte ranges may split multi-byte characters; assemble bytes.
    let mut output = Vec::<u8>::new();
    let mut base = 0usize;
    let mut nonempty_at_offset = false;
    while let Some(found) = regex_find_next(&re, &input_bytes, base, nonempty_at_offset, old_policy)
    {
        clear_matches(interp);
        store_matches(interp, &input, &found);

        let (start, end) = found.groups[0].unwrap_or((base, base));
        output.extend_from_slice(&input_bytes[base..start]);

        for replacement in &replacements {
            match replacement {
                Segment::Literal(text) => output.extend_from_slice(&crate::bytes::encode(text)),
                Segment::Group(n) => {
                    if *n > num_groups {
                        return err(format!(
                            "sub-command REGEX, mode REPLACE: replace expression \"{replace}\" \
                             contains an out-of-range escape for regex \"{regex}\"."
                        ));
                    }
                    if let Some(Some((s, e))) = found.groups.get(*n) {
                        output.extend_from_slice(&input_bytes[*s..*e]);
                    }
                }
            }
        }

        base = end;
        if start == input_bytes.len() {
            break;
        }
        nonempty_at_offset = start == end;
    }

    output.extend_from_slice(&input_bytes[base..]);
    interp.set_def(outvar.clone(), crate::bytes::decode(&output));
    Ok(())
}
