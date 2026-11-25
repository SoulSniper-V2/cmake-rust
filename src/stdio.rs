//! Terminal detection and attributed standard output, mirroring
//! `cmStdIoStream.cxx` and `cmStdIoTerminal.cxx` on Unix.

use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TermKind {
    None,
    VT100,
}

/// `cm::StdIo::TermAttr`. The declaration order is significant: enum-set
/// iteration emits escape sequences in this order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TermAttr {
    Normal,
    ForegroundBold,
    ForegroundBlack,
    ForegroundBlue,
    ForegroundCyan,
    ForegroundGreen,
    ForegroundMagenta,
    ForegroundRed,
    ForegroundWhite,
    ForegroundYellow,
    BackgroundBold,
    BackgroundBlack,
    BackgroundBlue,
    BackgroundCyan,
    BackgroundGreen,
    BackgroundMagenta,
    BackgroundRed,
    BackgroundWhite,
    BackgroundYellow,
}

impl TermAttr {
    pub const fn bit(self) -> TermAttrSet {
        1 << (self as u32)
    }

    const fn code(self) -> &'static str {
        match self {
            Self::Normal => "\x1b[0m",
            Self::ForegroundBold => "\x1b[1m",
            Self::ForegroundBlack => "\x1b[30m",
            Self::ForegroundBlue => "\x1b[34m",
            Self::ForegroundCyan => "\x1b[36m",
            Self::ForegroundGreen => "\x1b[32m",
            Self::ForegroundMagenta => "\x1b[35m",
            Self::ForegroundRed => "\x1b[31m",
            Self::ForegroundWhite => "\x1b[37m",
            Self::ForegroundYellow => "\x1b[33m",
            Self::BackgroundBold => "",
            Self::BackgroundBlack => "\x1b[40m",
            Self::BackgroundBlue => "\x1b[44m",
            Self::BackgroundCyan => "\x1b[46m",
            Self::BackgroundGreen => "\x1b[42m",
            Self::BackgroundMagenta => "\x1b[45m",
            Self::BackgroundRed => "\x1b[41m",
            Self::BackgroundWhite => "\x1b[47m",
            Self::BackgroundYellow => "\x1b[43m",
        }
    }
}

/// A bit set corresponding to `cm::enum_set<TermAttr>`.
pub type TermAttrSet = u32;

pub const NO_ATTRS: TermAttrSet = 0;

const ALL_ATTRS: [TermAttr; 19] = [
    TermAttr::Normal,
    TermAttr::ForegroundBold,
    TermAttr::ForegroundBlack,
    TermAttr::ForegroundBlue,
    TermAttr::ForegroundCyan,
    TermAttr::ForegroundGreen,
    TermAttr::ForegroundMagenta,
    TermAttr::ForegroundRed,
    TermAttr::ForegroundWhite,
    TermAttr::ForegroundYellow,
    TermAttr::BackgroundBold,
    TermAttr::BackgroundBlack,
    TermAttr::BackgroundBlue,
    TermAttr::BackgroundCyan,
    TermAttr::BackgroundGreen,
    TermAttr::BackgroundMagenta,
    TermAttr::BackgroundRed,
    TermAttr::BackgroundWhite,
    TermAttr::BackgroundYellow,
];

// List of known `TERM` names that support VT100 escape sequences, sorted as
// in cmStdIoStream.cxx.
const VT100_NAMES: [&str; 56] = [
    "Eterm",
    "alacritty",
    "alacritty-direct",
    "ansi",
    "color-xterm",
    "con132x25",
    "con132x30",
    "con132x43",
    "con132x60",
    "con80x25",
    "con80x28",
    "con80x30",
    "con80x43",
    "con80x50",
    "con80x60",
    "cons25",
    "console",
    "cygwin",
    "dtterm",
    "eterm-color",
    "gnome",
    "gnome-256color",
    "konsole",
    "konsole-256color",
    "kterm",
    "linux",
    "linux-c",
    "mach-color",
    "mlterm",
    "msys",
    "putty",
    "putty-256color",
    "rxvt",
    "rxvt-256color",
    "rxvt-cygwin",
    "rxvt-cygwin-native",
    "rxvt-unicode",
    "rxvt-unicode-256color",
    "screen",
    "screen-256color",
    "screen-256color-bce",
    "screen-bce",
    "screen-w",
    "screen.linux",
    "st-256color",
    "tmux",
    "tmux-256color",
    "vt100",
    "xterm",
    "xterm-16color",
    "xterm-256color",
    "xterm-88color",
    "xterm-color",
    "xterm-debian",
    "xterm-kitty",
    "xterm-termite",
];

fn env_bytes(name: &str) -> Option<Vec<u8>> {
    std::env::var_os(name).map(|value| value.as_os_str().as_bytes().to_vec())
}

fn nonzero_env(name: &str) -> bool {
    env_bytes(name).is_some_and(|value| !value.is_empty() && value != b"0")
}

fn term_env() -> Option<TermKind> {
    // Disable color according to https://bixense.com/clicolors/ convention.
    if env_bytes("NO_COLOR").is_some_and(|value| !value.is_empty() && value != b"0") {
        return Some(TermKind::None);
    }
    // Force color according to the same convention.
    if nonzero_env("CLICOLOR_FORCE") {
        return Some(TermKind::VT100);
    }
    // Disable color according to the same convention.
    if env_bytes("CLICOLOR").is_some_and(|value| value == b"0") {
        return Some(TermKind::None);
    }
    // GNU make 4.1+ may tell us that its output is destined for a TTY.
    if env_bytes("MAKE_TERMOUT").is_some_and(|value| !value.is_empty()) {
        return Some(TermKind::VT100);
    }
    None
}

fn term_is_vt100() -> bool {
    let Some(term) = std::env::var_os("TERM") else {
        return false;
    };
    let Some(term) = term.to_str() else {
        return false;
    };
    VT100_NAMES.binary_search(&term).is_ok()
}

fn stream_kind(fd: libc::c_int) -> TermKind {
    if unsafe { libc::isatty(fd) } == 1 && term_is_vt100() {
        TermKind::VT100
    } else {
        TermKind::None
    }
}

static TERM_ENV: OnceLock<Option<TermKind>> = OnceLock::new();
static OUT_KIND: OnceLock<TermKind> = OnceLock::new();
static ERR_KIND: OnceLock<TermKind> = OnceLock::new();

fn term_env_cached() -> Option<TermKind> {
    *TERM_ENV.get_or_init(term_env)
}

fn out_kind() -> TermKind {
    *OUT_KIND.get_or_init(|| stream_kind(libc::STDOUT_FILENO))
}

fn err_kind() -> TermKind {
    *ERR_KIND.get_or_init(|| stream_kind(libc::STDERR_FILENO))
}

/// Initialize process stream metadata near program entry, as constructing
/// `cm::StdIo::Console` does in cmakemain.
pub fn init() {
    let _ = term_env_cached();
    let _ = out_kind();
    let _ = err_kind();
}

fn write_attrs(out: &mut impl Write, attrs: TermAttrSet) -> io::Result<()> {
    for attr in ALL_ATTRS {
        if attrs & attr.bit() != 0 {
            out.write_all(attr.code().as_bytes())?;
        }
    }
    Ok(())
}

fn print_to(out: &mut impl Write, kind: TermKind, attrs: TermAttrSet, text: &str) {
    let kind = term_env_cached().unwrap_or(kind);
    if kind == TermKind::VT100 {
        let _ = write_attrs(out, attrs);
        let _ = out.write_all(text.as_bytes());
        let _ = write_attrs(out, TermAttr::Normal.bit());
    } else {
        let _ = out.write_all(text.as_bytes());
    }
}

/// `cm::StdIo::Print(cm::StdIo::Out(), attrs, text)`.
pub fn print_out(attrs: TermAttrSet, text: &str) {
    let mut out = io::stdout().lock();
    print_to(&mut out, out_kind(), attrs, text);
    let _ = out.flush();
}

/// Print attributed stderr text followed by uncolored bytes. This is the
/// shape of cmakemainMessageCallback, whose trailing call stack and newline
/// are outside the attributed `Print` call.
pub fn print_err_then_plain(attrs: TermAttrSet, text: &str, plain: &str) {
    let mut err = io::stderr().lock();
    print_to(&mut err, err_kind(), attrs, text);
    let _ = err.write_all(plain.as_bytes());
    let _ = err.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vt100_names_are_sorted() {
        assert!(VT100_NAMES.is_sorted());
    }

    #[test]
    fn attr_order() {
        let attrs = TermAttr::ForegroundMagenta.bit() | TermAttr::ForegroundBold.bit();
        let mut out = Vec::new();
        write_attrs(&mut out, attrs).unwrap();
        assert_eq!(out, b"\x1b[1m\x1b[35m");
    }
}
