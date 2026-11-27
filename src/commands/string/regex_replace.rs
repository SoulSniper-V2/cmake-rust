//! Port of `cmStringReplaceHelper` (REGEX REPLACE `\N` back-references)
//! and the `RegexReplace` handler from `cmStringCommand.cxx`.

use crate::cmregex::{Regex, ReplacePart, parse_replacement};
use crate::interp::{CmdResult, Interp, err};
use crate::state::{clear_matches, store_matches};

use super::{cm_join, old_bol, regex_find_next};

pub fn regex_replace(interp: &mut Interp, args: &[String]) -> CmdResult {
    let regex = &args[2];
    let replace = &args[3];
    let outvar = &args[4];

    let compiled = Regex::new(regex);
    let replacements = match parse_replacement(replace) {
        Ok(replacements) => replacements,
        Err(error) => {
            return err(format!("sub-command REGEX, mode REPLACE: {error}."));
        }
    };

    clear_matches(interp);

    let re = match compiled {
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
                ReplacePart::Literal(text) => output.extend_from_slice(&crate::bytes::encode(text)),
                ReplacePart::Capture(n) => {
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
