//! Port of `cmStringReplaceHelper` (REGEX REPLACE `\N` back-references)
//! and the `RegexReplace` handler from `cmStringCommand.cxx`.

use crate::interp::{CmdResult, Interp, err};
use crate::state::{clear_matches, store_matches};

use super::{cm_join, old_bol, regex_find_next};

pub enum Segment {
    Literal(String),
    Group(usize),
}

/// `cmStringReplaceHelper::ParseReplaceExpression`. Returns the error text on
/// failure.
pub fn parse_replace_expression(expr: &str) -> Result<Vec<Segment>, String> {
    let bytes = expr.as_bytes();
    let mut replacements = Vec::new();
    let mut l = 0usize;
    while l < bytes.len() {
        let found = expr[l..].find('\\').map(|r| l + r);
        let Some(mut r) = found else {
            replacements.push(Segment::Literal(expr[l..].to_owned()));
            break;
        };
        if r - l > 0 {
            replacements.push(Segment::Literal(expr[l..r].to_owned()));
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
            return Err(format!(
                "Unknown escape \"{}\" in replace-expression",
                &expr[r..r + 2]
            ));
        }
        r += 2;
        l = r;
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

    let mut output = String::new();
    let mut base = 0usize;
    let mut nonempty_at_offset = false;
    while let Some(found) = regex_find_next(&re, &input, base, nonempty_at_offset, old_policy) {
        clear_matches(interp);
        store_matches(interp, &input, &found);

        let (start, end) = found.groups[0].unwrap_or((base, base));
        output.push_str(input.get(base..start).unwrap_or(""));

        for replacement in &replacements {
            match replacement {
                Segment::Literal(text) => output.push_str(text),
                Segment::Group(n) => {
                    if *n > num_groups {
                        return err(format!(
                            "sub-command REGEX, mode REPLACE: replace expression \"{replace}\" \
                             contains an out-of-range escape for regex \"{regex}\"."
                        ));
                    }
                    if let Some(Some((s, e))) = found.groups.get(*n) {
                        output.push_str(input.get(*s..*e).unwrap_or(""));
                    }
                }
            }
        }

        base = end;
        if start == input.len() {
            break;
        }
        nonempty_at_offset = start == end;
    }

    output.push_str(input.get(base..).unwrap_or(""));
    interp.set_def(outvar.clone(), output);
    Ok(())
}
