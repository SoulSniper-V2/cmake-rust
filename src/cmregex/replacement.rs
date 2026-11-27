//! Replacement expressions used by the commands backed by cmStringReplaceHelper.

pub(crate) enum ReplacePart {
    Literal(String),
    Capture(usize),
}

/// Parse replacement escapes as bytes, including the one byte after a
/// backslash in an unknown-escape diagnostic.
pub(crate) fn parse_replacement(expression: &str) -> Result<Vec<ReplacePart>, String> {
    let encoded = crate::bytes::encode(expression);
    let mut rest = encoded.as_ref();
    let mut parts = Vec::new();
    while !rest.is_empty() {
        let Some(slash) = rest.iter().position(|&c| c == b'\\') else {
            parts.push(ReplacePart::Literal(crate::bytes::decode(rest)));
            break;
        };
        if slash > 0 {
            parts.push(ReplacePart::Literal(crate::bytes::decode(&rest[..slash])));
        }
        let Some(&next) = rest.get(slash + 1) else {
            return Err("replace-expression ends in a backslash".into());
        };
        parts.push(match next {
            b'0'..=b'9' => ReplacePart::Capture(usize::from(next - b'0')),
            b'n' => ReplacePart::Literal("\n".into()),
            b'\\' => ReplacePart::Literal("\\".into()),
            _ => {
                let escape = crate::bytes::decode(&rest[slash..slash + 2]);
                return Err(format!("Unknown escape \"{escape}\" in replace-expression"));
            }
        });
        rest = &rest[slash + 2..];
    }
    Ok(parts)
}
