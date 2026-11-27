//! Shell escaping (port of `cmOutputConverter::Shell_GetArgument` and
//! friends).

pub const FLAG_MAKE: u32 = 1 << 0;
pub const FLAG_VSIDE: u32 = 1 << 1;
pub const FLAG_ECHO_WINDOWS: u32 = 1 << 2;
pub const FLAG_WATCOM_WMAKE: u32 = 1 << 3;
pub const FLAG_MINGW_MAKE: u32 = 1 << 4;
pub const FLAG_NMAKE: u32 = 1 << 5;
pub const FLAG_ALLOW_MAKE_VARIABLES: u32 = 1 << 6;
pub const FLAG_WATCOM_QUOTE: u32 = 1 << 7;
pub const FLAG_IS_UNIX: u32 = 1 << 8;
pub const FLAG_UNESCAPE_NINJA_CONFIGURATION: u32 = 1 << 9;
pub const FLAG_IS_RESPONSE: u32 = 1 << 10;
pub const FLAG_NINJA: u32 = 1 << 11;

/// Unix Makefiles flags for shell commands.
pub const MAKE_UNIX: u32 = FLAG_MAKE | FLAG_IS_UNIX;

/// Flags used by the Ninja generator's local generator on Unix
/// (`cmOutputConverter::EscapeForShell` with default arguments).
pub const NINJA_UNIX: u32 = MAKE_UNIX | FLAG_NINJA;

fn is_shell_operator(text: &str) -> bool {
    matches!(
        text,
        "<" | ">" | "<<" | ">>" | "|" | "||" | "&&" | "&>" | "1>" | "2>" | "2>&1" | "1>&2"
    )
}

fn is_whitespace(byte: u8) -> bool {
    byte == b' ' || byte == b'\t'
}

fn needs_quotes_unix(byte: u8) -> bool {
    matches!(
        byte,
        b'\''
            | b'`'
            | b';'
            | b'#'
            | b'&'
            | b'$'
            | b'('
            | b')'
            | b'~'
            | b'<'
            | b'>'
            | b'|'
            | b'*'
            | b'^'
            | b'\\'
    )
}

fn needs_quotes_windows(byte: u8) -> bool {
    matches!(byte, b'\'' | b'#' | b'&' | b'<' | b'>' | b'|' | b'^')
}

fn is_make_variable_name(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphabetic()
}

fn char_needs_quotes(byte: u8, flags: u32) -> bool {
    if flags & FLAG_IS_UNIX == 0 && flags & FLAG_ECHO_WINDOWS != 0 {
        return false;
    }
    if is_whitespace(byte) {
        return true;
    }
    if flags & FLAG_IS_RESPONSE != 0 && byte == b'-' {
        return true;
    }
    if flags & FLAG_IS_UNIX != 0 {
        needs_quotes_unix(byte)
    } else {
        needs_quotes_windows(byte) || (byte == b';' && flags & FLAG_VSIDE != 0)
    }
}

fn skip_make_variables(bytes: &[u8], mut index: usize) -> usize {
    while index + 1 < bytes.len() && bytes[index] == b'$' && bytes[index + 1] == b'(' {
        let mut skip = index + 2;
        while skip < bytes.len() && is_make_variable_name(bytes[skip]) {
            skip += 1;
        }
        if skip < bytes.len() && bytes[skip] == b')' {
            index = skip + 1;
        } else {
            break;
        }
    }
    index
}

fn argument_needs_quotes(input: &[u8], flags: u32) -> bool {
    if input.is_empty() {
        return true;
    }
    let mut index = 0;
    while index < input.len() {
        if flags & FLAG_ALLOW_MAKE_VARIABLES != 0 {
            index = skip_make_variables(input, index);
            if index >= input.len() {
                break;
            }
        }
        if char_needs_quotes(input[index], flags) {
            return true;
        }
        index += 1;
    }
    if flags & FLAG_IS_UNIX != 0
        && input.len() == 1
        && matches!(input[0], b'?' | b'&' | b'^' | b'|' | b'#')
    {
        return true;
    }
    if flags & FLAG_MINGW_MAKE != 0
        && flags & FLAG_MAKE != 0
        && input.len() > 1
        && input[0] == b'\\'
        && input[1] == b'\\'
    {
        return true;
    }
    false
}

/// `cmOutputConverter::Shell_GetArgument`.
pub fn shell_get_argument(input: &str, flags: u32) -> String {
    let encoded = crate::bytes::encode(input);
    let bytes = encoded.as_ref();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len() + 2);
    let mut windows_backslashes = 0usize;
    let need_quotes = argument_needs_quotes(bytes, flags);
    if need_quotes {
        if flags & FLAG_WATCOM_QUOTE != 0 {
            if flags & FLAG_IS_UNIX != 0 {
                out.push(b'"');
            }
            out.push(b'\'');
        } else {
            out.push(b'"');
        }
    }
    let mut index = 0;
    while index < bytes.len() {
        if flags & FLAG_ALLOW_MAKE_VARIABLES != 0 {
            let skip = skip_make_variables(bytes, index);
            if skip != index {
                out.extend_from_slice(&bytes[index..skip]);
                index = skip;
                windows_backslashes = 0;
                if index >= bytes.len() {
                    break;
                }
            }
        }
        let byte = bytes[index];
        if flags & FLAG_IS_UNIX != 0 {
            if matches!(byte, b'\\' | b'"' | b'`' | b'$') {
                out.push(b'\\');
            }
        } else if flags & FLAG_ECHO_WINDOWS != 0 {
        } else if byte == b'\\' {
            windows_backslashes += 1;
        } else if byte == b'"' {
            while windows_backslashes > 0 {
                windows_backslashes -= 1;
                out.push(b'\\');
            }
            out.push(b'\\');
        } else {
            windows_backslashes = 0;
        }
        match byte {
            b'$' => {
                if flags & FLAG_MAKE != 0 {
                    out.extend_from_slice(b"$$");
                } else if flags & FLAG_VSIDE != 0 {
                    out.extend_from_slice(b"\"$\"");
                } else {
                    out.push(b'$');
                }
            }
            b'#' => {
                if flags & FLAG_MAKE != 0 && flags & FLAG_WATCOM_WMAKE != 0 {
                    out.extend_from_slice(b"$#");
                } else {
                    out.push(b'#');
                }
            }
            b'%' => {
                if flags & FLAG_VSIDE != 0
                    || (flags & FLAG_MAKE != 0
                        && (flags & FLAG_MINGW_MAKE != 0 || flags & FLAG_NMAKE != 0))
                {
                    out.extend_from_slice(b"%%");
                } else {
                    out.push(b'%');
                }
            }
            b';' => {
                if flags & FLAG_VSIDE != 0 {
                    out.extend_from_slice(b"\";\"");
                } else {
                    out.push(b';');
                }
            }
            b'\n' => {
                if flags & FLAG_NINJA != 0 {
                    out.extend_from_slice(b"$\n");
                } else {
                    out.push(b'\n');
                }
            }
            other => out.push(other),
        }
        index += 1;
    }
    if need_quotes {
        while windows_backslashes > 0 {
            windows_backslashes -= 1;
            out.push(b'\\');
        }
        if flags & FLAG_WATCOM_QUOTE != 0 {
            out.push(b'\'');
            if flags & FLAG_IS_UNIX != 0 {
                out.push(b'"');
            }
        } else {
            out.push(b'"');
        }
    }
    let mut result = crate::bytes::decode(&out);
    if flags & FLAG_UNESCAPE_NINJA_CONFIGURATION != 0 {
        let pattern = if flags & FLAG_IS_UNIX != 0 {
            "\\$${CONFIGURATION}"
        } else {
            "$${CONFIGURATION}"
        };
        result = result.replace(pattern, "${CONFIGURATION}");
    }
    result
}

/// Static `cmOutputConverter::EscapeForShell(str, flags)`.
pub fn escape_for_shell(input: &str, flags: u32) -> String {
    if is_shell_operator(input) {
        return input.to_owned();
    }
    shell_get_argument(input, flags)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_quoting() {
        assert_eq!(escape_for_shell("a b", FLAG_IS_UNIX), "\"a b\"");
        assert_eq!(escape_for_shell("plain", FLAG_IS_UNIX), "plain");
        assert_eq!(escape_for_shell("", FLAG_IS_UNIX), "\"\"");
        assert_eq!(escape_for_shell("$x", NINJA_UNIX), "\"\\$$x\"");
        assert_eq!(escape_for_shell("-DA=\"b\"", FLAG_IS_UNIX), "-DA=\\\"b\\\"");
        assert_eq!(escape_for_shell("&&", FLAG_IS_UNIX), "&&");
    }
}
