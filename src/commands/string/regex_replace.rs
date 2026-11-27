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
                    if *n > re.num_groups() {
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
