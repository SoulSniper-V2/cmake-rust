//! Faithful Rust port of the regular-expression engine bundled with CMake:
//! kwsys `RegularExpression`, the Henry Spencer-derived backtracking engine
//! that powers `string(REGEX ...)`, `if(... MATCHES ...)`,
//! `list(FILTER ... REGEX)`, `file(STRINGS ... REGEX)` and friends.  The
//! reference implementation is `Source/kwsys/RegularExpression.cxx` plus
//! `RegularExpression.hxx.in` in CMake 4.1.0; this file ports its
//! compile -> bytecode -> backtracking-match structure one to one.
//!
//! # Engine notes
//!
//! * Everything operates on **bytes**, not Unicode. `.` matches any single
//!   byte except `NUL`, `[...]` sets are byte sets, and the offsets in
//!   [`Match`] are byte offsets into the searched text.
//! * The pattern is treated like a C string: it ends at the first `NUL`
//!   byte, exactly like `compile(char const*)`. The searched text is
//!   scanned with C-string semantics too: any `NUL` byte acts like the end
//!   of the input for `EOL`, `ANY`, class and literal matching, and the
//!   start-position scan loops stop at it (mirroring `strchr`/`strncmp`/
//!   `strlen` on `char const*`). Searching may still resume *after* a
//!   `NUL` when `find_at` is called with `start` beyond it.
//! * Compile errors and messages mirror the C++ diagnostics:
//!   "Too many parentheses." (more than `NSUBEXP - 1` = 31 `()` groups -
//!   kwsys 4.1.0 keeps 32 match slots internally even though only groups
//!   0-9 are exposed through `CMAKE_MATCH_*` and therefore through
//!   [`Match`]), "*+ operand could be empty.", "Nested *?+.",
//!   "Invalid range in [].", "Unmatched [].", "?+* follows nothing.",
//!   "Trailing backslash.", "Unmatched parentheses.",
//!   "Expression too big.".
//! * Semantics are greedy, leftmost-first backtracking: alternation tries
//!   branches left to right (so `a|ab` never yields `ab`), `*`/`+`/`?` are
//!   greedy, and a capture group inside a loop reports its **last**
//!   iteration's span.
//! * `\<char>` always quotes the next byte literally: `\n` is a literal
//!   `n`, `\t` a literal `t`, there are no class shorthands. Inside
//!   `[...]` a backslash is an ordinary byte; `]` ends the class unless it
//!   is the first element (or first after `^`), and `-` is literal at the
//!   edges.
//! * `^` and `$` anchor at the absolute beginning/end of the input only;
//!   a newline byte has no special meaning.
//!
//! # `find_at`, `^` and CMP0186 (what a caller must do)
//!
//! In CMake 4.1.0 `RegularExpression::find(string, rmatch, offset,
//! options)` accepts two option bits:
//!   * `BOL_AT_OFFSET` - sets `regbol = string + offset`, so `^` matches
//!     at the search offset;
//!   * `NONEMPTY_AT_OFFSET` - sets `regreject = string + offset`, so a
//!     match may not *end* exactly at the offset. `string(REGEX
//!     MATCHALL)` and `string(REGEX REPLACE)` use this to force progress
//!     after an empty match.
//!
//! [`Regex::find_at`]'s `not_bol` maps onto the anchor bit:
//! `not_bol == false` passes `BOL_AT_OFFSET` (the CMP0186 *old* behavior),
//! `not_bol == true` passes no anchor option (CMP0186 *new* behavior: `^`
//! only matches at absolute position 0). `Regex::find` always searches
//! from offset 0 where the two coincide.
//!
//! A `MATCHALL`/`REPLACE` caller must therefore, mirroring CMake's own
//! loops (`RegexMatchAll` in `cmStringCommand.cxx` and
//! `cmStringReplaceHelper::Replace`):
//!   1. keep `base = 0`; search at `base`; on a match emit the unmatched
//!      gap `input[base .. start]` (REPLACE) or record the match
//!      (MATCHALL), then set `base = match end`;
//!   2. stop when `match start == input.len()` (an empty match at the very
//!      end is still reported once);
//!   3. after an *empty* match (`start == end`), the next search needs
//!      `NONEMPTY_AT_OFFSET` so the same empty match is not found again -
//!      the engine supports it internally via `find_opts`; a caller only
//!      needs a one-line flag addition alongside `not_bol` to expose it;
//!   4. pass `not_bol = false` under CMP0186 OLD so `^` re-matches at
//!      every continuation offset, `not_bol = true` under NEW.

// Opcode values; they are part of the compiled-program encoding and are
// kept identical to the C++ source.
const END: u8 = 0;
const BOL: u8 = 1;
const EOL: u8 = 2;
const ANY: u8 = 3;
const ANYOF: u8 = 4;
const ANYBUT: u8 = 5;
const BRANCH: u8 = 6;
const BACK: u8 = 7;
const EXACTLY: u8 = 8;
const NOTHING: u8 = 9;
const STAR: u8 = 10;
const PLUS: u8 = 11;
const OPEN: u8 = 20; // OPEN+1 .. OPEN+NSUBEXP mark group starts.
const CLOSE: u8 = 52; // CLOSE+1 .. CLOSE+NSUBEXP mark group ends.
const MAGIC: u8 = 0o234;

// kwsys `RegularExpressionMatch::NSUBEXP`: 32 internal capture slots in
// CMake 4.1.0 (so at most 31 `()` groups compile). Only slots 0..=9 are
// observable through `CMAKE_MATCH_*`, so [`Match`] exposes 10 groups.
const NSUBEXP: usize = 32;
const MAX_GROUPS: usize = 10;

// Flags passed up and down during compilation.
const HASWIDTH: u8 = 0o1; // Known never to match null string.
const SIMPLE: u8 = 0o2; // Simple enough to be STAR/PLUS operand.
const SPSTART: u8 = 0o4; // Starts with * or +.
const WORST: u8 = 0; // Worst case.

// Metacharacters that terminate a literal run (strcspn(regparse, META)).
const META: &[u8] = b"^$.[()|?+*\\";

// `find` option bits, identical to `RegularExpression::Options`.
const BOL_AT_OFFSET: u8 = 1;
const NONEMPTY_AT_OFFSET: u8 = 2;

fn ismult(c: u8) -> bool {
    c == b'*' || c == b'+' || c == b'?'
}

/// The result of a successful search. `groups[n]` is the byte range
/// `[start, end)` of capture group `n` in the searched text; `groups[0]`
/// is the whole match. `None` means the group did not participate (or, for
/// `n > 0`, that it is beyond the 10 slots CMake exposes through
/// `CMAKE_MATCH_0`..`CMAKE_MATCH_9`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Match {
    pub groups: [Option<(usize, usize)>; MAX_GROUPS],
}

impl Match {
    /// The text of group `n` in `text`.  Groups are byte ranges and may split
    /// a multi-byte character; such bytes are kept lossily instead of
    /// panicking on a char boundary.
    pub fn text<'a>(&self, text: &'a str, n: usize) -> Option<std::borrow::Cow<'a, str>> {
        self.groups[n].map(|(start, end)| String::from_utf8_lossy(&text.as_bytes()[start..end]))
    }
}

/// A compiled regular expression (kwsys `program` plus its precomputed
/// match hints).
#[derive(Clone)]
pub struct Regex {
    program: Vec<u8>,
    regstart: u8,             // byte that must begin a match; 0 = none obvious
    reganch: bool,            // match is anchored at beginning-of-line
    regmust: Option<Vec<u8>>, // literal that must appear in any match
}

impl Regex {
    /// Compile `pattern`. Mirrors `RegularExpression::compile(char
    /// const*)`, including the C-string convention that the pattern ends
    /// at its first NUL byte.
    pub fn new(pattern: &str) -> Result<Regex, String> {
        let mut comp = Compiler {
            pat: c_str(pattern.as_bytes()),
            parse: 0,
            regnpar: 1,
            code: Vec::new(),
        };
        comp.regc(MAGIC);
        // kwsys prints compile diagnostics to stdout (printf).
        let (_, flags) = match comp.reg(false) {
            Ok(result) => result,
            Err(error) => {
                println!("{error}");
                println!("RegularExpression::compile(): Error in compile.");
                return Err(error);
            }
        };
        if comp.code.len() >= 65535 {
            println!("RegularExpression::compile(): Expression too big.");
            return Err("RegularExpression::compile(): Expression too big.".to_string());
        }

        let mut re = Regex {
            program: comp.code,
            regstart: 0,
            reganch: false,
            regmust: None,
        };

        // Dig out information for optimizations (mirrors compile()).
        let mut scan = 1usize; // First BRANCH.
        if regnext(&re.program, scan).is_some_and(|n| re.program[n] == END) {
            // Only one top-level choice.
            scan += 3; // OPERAND(scan)

            // Starting-point info.
            match re.program[scan] {
                EXACTLY => re.regstart = re.program[scan + 3],
                BOL => re.reganch = true,
                _ => {}
            }

            // If there's something expensive in the r.e., find the longest
            // literal string that must appear and make it the regmust.
            // Ties resolve in favor of later strings.
            if flags & SPSTART != 0 {
                let mut longest = None;
                let mut len = 0usize;
                let mut s = Some(scan);
                while let Some(n) = s {
                    if re.program[n] == EXACTLY && operand(&re.program, n).len() >= len {
                        len = operand(&re.program, n).len();
                        longest = Some(n);
                    }
                    s = regnext(&re.program, n);
                }
                re.regmust = longest.map(|n| operand(&re.program, n).to_vec());
            }
        }
        Ok(re)
    }

    /// Search `text` starting at byte offset `start`. `not_bol` selects
    /// how `^` behaves at `start`: see the module docs for the mapping to
    /// kwsys `find` options and CMP0186.
    pub fn find_at(&self, text: &str, start: usize, not_bol: bool) -> Option<Match> {
        let options = if not_bol { 0 } else { BOL_AT_OFFSET };
        self.find_opts(text.as_bytes(), start, options)
    }

    /// `RegularExpression::find(text, offset, options)` with explicit
    /// BOL_AT_OFFSET / NONEMPTY_AT_OFFSET flags (the repeated-search loops
    /// of string(REGEX MATCHALL/REPLACE)).
    pub fn find_with(
        &self,
        text: &str,
        offset: usize,
        bol_at_offset: bool,
        nonempty_at_offset: bool,
    ) -> Option<Match> {
        let mut options = 0;
        if bol_at_offset {
            options |= BOL_AT_OFFSET;
        }
        if nonempty_at_offset {
            options |= NONEMPTY_AT_OFFSET;
        }
        self.find_opts(text.as_bytes(), offset, options)
    }

    /// Search `text` from the beginning.
    pub fn find(&self, text: &str) -> Option<Match> {
        self.find_at(text, 0, false)
    }

    /// [`find_at`] over raw bytes: `RegularExpression::find` on a
    /// `char const*` that points `start` bytes into `text` (so `^` anchors
    /// at `start` unless `not_bol`).
    pub fn find_bytes(&self, text: &[u8], start: usize, not_bol: bool) -> Option<Match> {
        let options = if not_bol { 0 } else { BOL_AT_OFFSET };
        self.find_opts(text, start, options)
    }

    /// `RegularExpression::find(string, rmatch, offset, options)`.
    fn find_opts(&self, text: &[u8], offset: usize, options: u8) -> Option<Match> {
        // Check validity of program.
        if self.program.first() != Some(&MAGIC) {
            return None;
        }

        let offset = offset.min(text.len());

        // If there is a "must appear" string, look for it.
        if let Some(must) = &self.regmust {
            if !contains_cstr(&text[offset..], must) {
                return None;
            }
        }

        let mut m = Matcher {
            program: &self.program,
            input: text,
            reginput: 0,
            // Mark beginning of line for ^ .
            regbol: if options & BOL_AT_OFFSET != 0 {
                offset
            } else {
                0
            },
            regreject: if options & NONEMPTY_AT_OFFSET != 0 {
                Some(offset)
            } else {
                None
            },
            regstartp: [None; NSUBEXP],
            regendp: [None; NSUBEXP],
        };

        // Simplest case: anchored match need be tried only once.
        if self.reganch {
            return m.regtry(offset).then(|| m.collect());
        }

        // Messy cases: unanchored match.
        if self.regstart != 0 {
            // We know what byte it must start with.
            let mut s = offset;
            while let Some(pos) = strchr_nonzero(text, s, self.regstart) {
                if m.regtry(pos) {
                    return Some(m.collect());
                }
                s = pos + 1;
            }
        } else {
            // We don't -- general case.
            let mut s = offset;
            loop {
                if m.regtry(s) {
                    return Some(m.collect());
                }
                if byte_at(text, s) == 0 {
                    break;
                }
                s += 1;
            }
        }
        None
    }
}

/// The `pattern[0 .. first NUL]` prefix: the C-string view `compile()`
/// and the scan loops have of a `char const*`.
fn c_str(bytes: &[u8]) -> &[u8] {
    match bytes.iter().position(|&b| b == 0) {
        Some(nul) => &bytes[..nul],
        None => bytes,
    }
}

/// `*p` for a `char const*` cursor: byte `i` or `'\0'` at/past the end.
fn byte_at(s: &[u8], i: usize) -> u8 {
    s.get(i).copied().unwrap_or(0)
}

/// `strchr(s + from, c)` for `c != '\0'`: the first position at/after
/// `from` holding byte `c`, or `None` when a `'\0'` is reached first.
fn strchr_nonzero(s: &[u8], from: usize, c: u8) -> Option<usize> {
    let i = s[from..]
        .iter()
        .position(|&b| b == 0 || b == c)
        .map_or(s.len(), |rel| from + rel);
    (byte_at(s, i) == c).then_some(i)
}

/// `strchr`/`strncmp` substring search: `needle` never contains `'\0'`, so
/// a `'\0'` byte in `hay` can never be part of a successful comparison.
fn contains_cstr(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

/// `NEXT(p)`: the node a node's "next" offset refers to (`None` when the
/// offset is 0). `BACK` nodes point backwards, all others forwards.
fn regnext(code: &[u8], p: usize) -> Option<usize> {
    let offset = (usize::from(code[p + 1]) << 8) | usize::from(code[p + 2]);
    if offset == 0 {
        return None;
    }
    if code[p] == BACK {
        p.checked_sub(offset)
    } else {
        Some(p + offset)
    }
}

/// `OPERAND(p)`: the NUL-terminated operand bytes of a node.
fn operand(code: &[u8], p: usize) -> &[u8] {
    let start = p + 3;
    let end = code[start..]
        .iter()
        .position(|&b| b == 0)
        .map_or(code.len(), |rel| start + rel);
    &code[start..end]
}

/////////////////////////////////////////////////////////////////////////
//
//  COMPILE AND ASSOCIATED FUNCTIONS
//
/////////////////////////////////////////////////////////////////////////

/// Mirrors `RegExpCompile`: pattern cursor, () count and code emitter.
/// The C++ code compiles twice (once into a dummy sink to learn the size,
/// once for real); emitting straight into a `Vec` makes the sizing pass
/// unnecessary while producing the identical program.
struct Compiler<'p> {
    pat: &'p [u8],
    parse: usize,   // regparse
    regnpar: usize, // () count
    code: Vec<u8>,  // emitted program; code.len() plays regcode/regsize
}

impl Compiler<'_> {
    /// `*regparse` (the pattern's terminating `'\0'` reads as 0).
    fn peek(&self) -> u8 {
        byte_at(self.pat, self.parse)
    }

    /// `*(regparse + i)` (i may be negative-ish via wrapping callers).
    fn peek_at(&self, i: usize) -> u8 {
        byte_at(self.pat, i)
    }

    /// `reg()` - regular expression, i.e. main body or parenthesized thing.
    /// Caller must absorb opening parenthesis.
    fn reg(&mut self, paren: bool) -> Result<(usize, u8), String> {
        let mut flagp = HASWIDTH; // Tentatively.
        let mut parno = 0usize;

        // Make an OPEN node, if parenthesized.
        let mut ret = usize::MAX; // stands in for nullptr
        if paren {
            if self.regnpar >= NSUBEXP {
                return Err("RegularExpression::compile(): Too many parentheses.".to_string());
            }
            parno = self.regnpar;
            self.regnpar += 1;
            ret = self.regnode(OPEN + parno as u8);
        }

        // Pick up the branches, linking them together.
        let (br, mut flags) = self.regbranch()?;
        if ret == usize::MAX {
            ret = br;
        } else {
            self.regtail(ret, br); // OPEN -> first.
        }
        if flags & HASWIDTH == 0 {
            flagp &= !HASWIDTH;
        }
        flagp |= flags & SPSTART;
        while self.peek() == b'|' {
            self.parse += 1;
            let (br2, flags2) = self.regbranch()?;
            self.regtail(ret, br2); // BRANCH -> BRANCH.
            flags = flags2;
            if flags & HASWIDTH == 0 {
                flagp &= !HASWIDTH;
            }
            flagp |= flags & SPSTART;
        }

        // Make a closing node, and hook it on the end.
        let ender = self.regnode(if paren { CLOSE + parno as u8 } else { END });
        self.regtail(ret, ender);

        // Hook the tails of the branches to the closing node.
        let mut br = Some(ret);
        while let Some(b) = br {
            self.regoptail(b, ender);
            br = regnext(&self.code, b);
        }

        // Check for proper termination.
        if paren {
            let c = self.peek();
            self.parse += 1;
            if c != b')' {
                return Err("RegularExpression::compile(): Unmatched parentheses.".to_string());
            }
        } else if self.peek() != 0 {
            return if self.peek() == b')' {
                Err("RegularExpression::compile(): Unmatched parentheses.".to_string())
            } else {
                Err("RegularExpression::compile(): Internal error.".to_string())
            };
        }
        Ok((ret, flagp))
    }

    /// `regbranch()` - one alternative of an | operator. Implements the
    /// concatenation operator.
    fn regbranch(&mut self) -> Result<(usize, u8), String> {
        let mut flagp = WORST; // Tentatively.

        let ret = self.regnode(BRANCH);
        let mut chain = None;
        while self.peek() != 0 && self.peek() != b'|' && self.peek() != b')' {
            let (latest, flags) = self.regpiece()?;
            flagp |= flags & HASWIDTH;
            match chain {
                None => flagp |= flags & SPSTART, // First piece.
                Some(c) => self.regtail(c, latest),
            }
            chain = Some(latest);
        }
        if chain.is_none() {
            // Loop ran zero times.
            self.regnode(NOTHING);
        }
        Ok((ret, flagp))
    }

    /// `regpiece()` - something followed by possible [*+?].
    fn regpiece(&mut self) -> Result<(usize, u8), String> {
        let (ret, flags) = self.regatom()?;

        let op = self.peek();
        if !ismult(op) {
            return Ok((ret, flags));
        }

        if flags & HASWIDTH == 0 && op != b'?' {
            return Err("RegularExpression::compile() : *+ operand could be empty.".to_string());
        }
        let flagp = if op != b'+' {
            WORST | SPSTART
        } else {
            WORST | HASWIDTH
        };

        if op == b'*' && flags & SIMPLE != 0 {
            self.reginsert(STAR, ret);
        } else if op == b'*' {
            // Emit x* as (x&|), where & means "self".
            self.reginsert(BRANCH, ret); // Either x
            let back = self.regnode(BACK);
            self.regoptail(ret, back); // and loop
            self.regoptail(ret, ret); // back
            let br = self.regnode(BRANCH); // or
            self.regtail(ret, br);
            let nothing = self.regnode(NOTHING); // null.
            self.regtail(ret, nothing);
        } else if op == b'+' && flags & SIMPLE != 0 {
            self.reginsert(PLUS, ret);
        } else if op == b'+' {
            // Emit x+ as x(&|), where & means "self".
            let next = self.regnode(BRANCH); // Either
            self.regtail(ret, next);
            let back = self.regnode(BACK);
            self.regtail(back, ret); // loop back
            let br = self.regnode(BRANCH); // or
            self.regtail(next, br);
            let nothing = self.regnode(NOTHING); // null.
            self.regtail(ret, nothing);
        } else if op == b'?' {
            // Emit x? as (x|)
            self.reginsert(BRANCH, ret); // Either x
            let br = self.regnode(BRANCH); // or
            self.regtail(ret, br);
            let next = self.regnode(NOTHING); // null.
            self.regtail(ret, next);
            self.regoptail(ret, next);
        }
        self.parse += 1;
        if ismult(self.peek()) {
            return Err("RegularExpression::compile(): Nested *?+.".to_string());
        }
        Ok((ret, flagp))
    }

    /// `regatom()` - the lowest level. Gobbles an entire sequence of
    /// ordinary characters into a single EXACTLY node when possible.
    fn regatom(&mut self) -> Result<(usize, u8), String> {
        let mut flagp = WORST; // Tentatively.

        let c = self.peek();
        self.parse += 1;
        let ret = match c {
            b'^' => self.regnode(BOL),
            b'$' => self.regnode(EOL),
            b'.' => {
                flagp |= HASWIDTH | SIMPLE;
                self.regnode(ANY)
            }
            b'[' => {
                let node = if self.peek() == b'^' {
                    // Complement of range.
                    self.parse += 1;
                    self.regnode(ANYBUT)
                } else {
                    self.regnode(ANYOF)
                };
                if self.peek() == b']' || self.peek() == b'-' {
                    let b = self.peek();
                    self.regc(b);
                    self.parse += 1;
                }
                while self.peek() != 0 && self.peek() != b']' {
                    if self.peek() == b'-' {
                        self.parse += 1;
                        if self.peek() == b']' || self.peek() == 0 {
                            self.regc(b'-');
                        } else {
                            let lo = u16::from(self.peek_at(self.parse - 2)) + 1;
                            let hi = u16::from(self.peek());
                            if lo > hi + 1 {
                                return Err("RegularExpression::compile(): Invalid range in []."
                                    .to_string());
                            }
                            for b in lo..=hi {
                                self.regc(b as u8);
                            }
                            self.parse += 1;
                        }
                    } else {
                        let b = self.peek();
                        self.regc(b);
                        self.parse += 1;
                    }
                }
                self.regc(0);
                if self.peek() != b']' {
                    return Err("RegularExpression::compile(): Unmatched [].".to_string());
                }
                self.parse += 1;
                flagp |= HASWIDTH | SIMPLE;
                node
            }
            b'(' => {
                let (r, flags) = self.reg(true)?;
                flagp |= flags & (HASWIDTH | SPSTART);
                r
            }
            0 | b'|' | b')' => {
                // Never here: regbranch stops before these.
                return Err("RegularExpression::compile(): Internal error.".to_string());
            }
            b'?' | b'+' | b'*' => {
                return Err("RegularExpression::compile(): ?+* follows nothing.".to_string());
            }
            b'\\' => {
                let e = self.peek();
                if e == 0 {
                    return Err("RegularExpression::compile(): Trailing backslash.".to_string());
                }
                self.parse += 1;
                let r = self.regnode(EXACTLY);
                self.regc(e);
                self.regc(0);
                flagp |= HASWIDTH | SIMPLE;
                r
            }
            _ => {
                self.parse -= 1;
                let mut len = self
                    .pat
                    .iter()
                    .skip(self.parse)
                    .position(|b| META.contains(b))
                    .unwrap_or(self.pat.len() - self.parse);
                if len == 0 {
                    return Err("RegularExpression::compile(): Internal error.".to_string());
                }
                let ender = self.peek_at(self.parse + len);
                if len > 1 && ismult(ender) {
                    len -= 1; // Back off clear of ?+* operand.
                }
                flagp |= HASWIDTH;
                if len == 1 {
                    flagp |= SIMPLE;
                }
                let r = self.regnode(EXACTLY);
                for _ in 0..len {
                    let b = self.peek();
                    self.regc(b);
                    self.parse += 1;
                }
                self.regc(0);
                r
            }
        };
        Ok((ret, flagp))
    }

    /// `regnode()` - emit a node; returns its location.
    fn regnode(&mut self, op: u8) -> usize {
        let ret = self.code.len();
        self.code.extend_from_slice(&[op, 0, 0]); // opcode + null "next"
        ret
    }

    /// `regc()` - emit a byte of code.
    fn regc(&mut self, b: u8) {
        self.code.push(b);
    }

    /// `reginsert()` - insert an operator in front of an already-emitted
    /// operand (relocates the operand three bytes forward).
    fn reginsert(&mut self, op: u8, opnd: usize) {
        self.code.splice(opnd..opnd, [op, 0, 0]);
    }

    /// `regtail()` - set the next-pointer at the end of a node chain.
    fn regtail(&mut self, p: usize, val: usize) {
        // Find last node.
        let mut scan = p;
        while let Some(t) = regnext(&self.code, scan) {
            scan = t;
        }
        let offset = if self.code[scan] == BACK {
            scan.wrapping_sub(val)
        } else {
            val.wrapping_sub(scan)
        } & 0xffff;
        self.code[scan + 1] = (offset >> 8) as u8;
        self.code[scan + 2] = offset as u8;
    }

    /// `regoptail()` - regtail on operand of first argument; nop if
    /// operandless ("operandless" and "op != BRANCH" are synonymous in
    /// practice).
    fn regoptail(&mut self, p: usize, val: usize) {
        if self.code[p] != BRANCH {
            return;
        }
        self.regtail(p + 3, val);
    }
}

////////////////////////////////////////////////////////////////////////
//
//  find and friends
//
////////////////////////////////////////////////////////////////////////

/// Mirrors `RegExpFind`: match state for one `find()` invocation.
struct Matcher<'a> {
    program: &'a [u8],
    input: &'a [u8],
    reginput: usize,          // String-input pointer.
    regbol: usize,            // Beginning of input, for ^ check.
    regreject: Option<usize>, // Reject matches ending here.
    regstartp: [Option<usize>; NSUBEXP],
    regendp: [Option<usize>; NSUBEXP],
}

impl Matcher<'_> {
    /// `*reginput` (reads `'\0'` at/past the end of the input).
    fn byte(&self, i: usize) -> u8 {
        byte_at(self.input, i)
    }

    /// `regtry()` - try match at specific point. Returns whether it
    /// succeeded; on success `regstartp`/`regendp` are populated.
    fn regtry(&mut self, s: usize) -> bool {
        self.reginput = s;
        self.regstartp = [None; NSUBEXP];
        self.regendp = [None; NSUBEXP];
        if self.regmatch(1) {
            self.regstartp[0] = Some(s);
            self.regendp[0] = Some(self.reginput);
            return true;
        }
        false
    }

    /// The observed result: `Match::groups` copies slots 0..=9, which is
    /// all `CMAKE_MATCH_0..9` can observe in CMake.
    fn collect(&self) -> Match {
        let mut groups = [None; MAX_GROUPS];
        for (i, g) in groups.iter_mut().enumerate() {
            if let (Some(s), Some(e)) = (self.regstartp[i], self.regendp[i]) {
                *g = Some((s, e));
            }
        }
        Match { groups }
    }

    /// `regmatch(prog)` on an optional node; the C++ code calls
    /// `regmatch(next)` where `next` may be a null `regnext` result, which
    /// lands on the "corrupted pointers" failure path.
    fn regmatch_opt(&mut self, prog: Option<usize>) -> bool {
        prog.is_some_and(|p| self.regmatch(p))
    }

    /// `regmatch()` - main matching routine. Mirrors the C++ recursion and
    /// its iterative scan over "ordinary" nodes.
    fn regmatch(&mut self, prog: usize) -> bool {
        let mut scan = prog;
        loop {
            let next = regnext(self.program, scan);
            match self.program[scan] {
                BOL => {
                    if self.reginput != self.regbol {
                        return false;
                    }
                }
                EOL => {
                    if self.byte(self.reginput) != 0 {
                        return false;
                    }
                }
                ANY => {
                    if self.byte(self.reginput) == 0 {
                        return false;
                    }
                    self.reginput += 1;
                }
                EXACTLY => {
                    let opnd = operand(self.program, scan);
                    // `strncmp` semantics: the operand contains no NUL, so
                    // any NUL or shortfall in the input fails the compare.
                    if !self
                        .input
                        .get(self.reginput..)
                        .is_some_and(|tail| tail.starts_with(opnd))
                    {
                        return false;
                    }
                    self.reginput += opnd.len();
                }
                ANYOF => {
                    let opnd = operand(self.program, scan);
                    match self.input.get(self.reginput) {
                        Some(c) if opnd.contains(c) => self.reginput += 1,
                        _ => return false,
                    }
                }
                ANYBUT => {
                    let opnd = operand(self.program, scan);
                    match self.input.get(self.reginput) {
                        Some(c) if !opnd.contains(c) => self.reginput += 1,
                        _ => return false,
                    }
                }
                NOTHING | BACK => {}
                op if (OPEN + 1..=OPEN + 32).contains(&op) => {
                    let no = usize::from(op - OPEN);
                    let save = self.reginput;
                    if self.regmatch_opt(next) {
                        // Don't set startp if some later invocation of the
                        // same parentheses already has.
                        if let Some(slot) = self.regstartp.get_mut(no) {
                            if slot.is_none() {
                                *slot = Some(save);
                            }
                        }
                        return true;
                    }
                    return false;
                }
                op if (CLOSE + 1..=CLOSE + 32).contains(&op) => {
                    let no = usize::from(op - CLOSE);
                    let save = self.reginput;
                    if self.regmatch_opt(next) {
                        // Don't set endp if some later invocation of the
                        // same parentheses already has.
                        if let Some(slot) = self.regendp.get_mut(no) {
                            if slot.is_none() {
                                *slot = Some(save);
                            }
                        }
                        return true;
                    }
                    return false;
                }
                BRANCH => {
                    if !matches!(next, Some(n) if self.program[n] == BRANCH) {
                        // No choice: chain straight into the operand.
                        scan += 3; // OPERAND(scan)
                        continue;
                    }
                    let mut br = scan;
                    loop {
                        let save = self.reginput;
                        if self.regmatch(br + 3) {
                            // OPERAND(br)
                            return true;
                        }
                        self.reginput = save;
                        match regnext(self.program, br) {
                            Some(n) if self.program[n] == BRANCH => br = n,
                            _ => return false,
                        }
                    }
                }
                STAR | PLUS => {
                    // Lookahead to avoid useless match attempts when we
                    // know what byte comes next.
                    let nextch = match next {
                        Some(n) if self.program[n] == EXACTLY => self.program[n + 3],
                        _ => 0,
                    };
                    let min_no: isize = if self.program[scan] == STAR { 0 } else { 1 };
                    let save = self.reginput;
                    let mut no = self.regrepeat(scan + 3) as isize; // OPERAND(scan)
                    while no >= min_no {
                        // If it could work, try it.
                        if (nextch == 0 || self.byte(self.reginput) == nextch)
                            && self.regmatch_opt(next)
                        {
                            return true;
                        }
                        // Couldn't or didn't -- back up.
                        no -= 1;
                        // `reginput = save + no`; the value is never read
                        // again once this returns false.
                        self.reginput = save.saturating_add_signed(no);
                    }
                    return false;
                }
                END => {
                    // Can't end a match at regreject (NONEMPTY_AT_OFFSET).
                    return self.regreject != Some(self.reginput);
                }
                _ => {
                    // "Internal error -- memory corrupted."
                    return false;
                }
            }
            match next {
                Some(n) => scan = n,
                // "Internal error -- corrupted pointers."
                None => return false,
            }
        }
    }

    /// `regrepeat()` - repeatedly match something simple, report how many.
    fn regrepeat(&mut self, p: usize) -> usize {
        let mut count = 0usize;
        let mut scan = self.reginput;
        match self.program[p] {
            ANY => {
                // strlen(scan): bytes up to the next NUL.
                count = self.input[scan..]
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(self.input.len() - scan);
                scan += count;
            }
            EXACTLY => {
                // The operand of a STAR/PLUS EXACTLY is always one byte.
                let c = self.program[p + 3];
                while self.byte(scan) == c {
                    count += 1;
                    scan += 1;
                }
            }
            ANYOF => {
                let opnd = operand(self.program, p);
                while self.byte(scan) != 0 && opnd.contains(&self.byte(scan)) {
                    count += 1;
                    scan += 1;
                }
            }
            ANYBUT => {
                let opnd = operand(self.program, p);
                while self.byte(scan) != 0 && !opnd.contains(&self.byte(scan)) {
                    count += 1;
                    scan += 1;
                }
            }
            _ => {
                // "Internal error." (called inappropriately)
                return 0;
            }
        }
        self.reginput = scan;
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        assert!(s.len().is_multiple_of(2));
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// Mirrors `RegexMatchAll` in cmStringCommand.cxx: repeated `find`
    /// with `BOL_AT_OFFSET` under CMP0186 OLD and the
    /// `NONEMPTY_AT_OFFSET` progression guard after empty matches.
    fn matchall(re: &Regex, input: &[u8], new_policy: bool) -> Vec<u8> {
        let mut output: Vec<u8> = Vec::new();
        let mut base = 0usize;
        let mut nonempty = false;
        loop {
            let options = (if new_policy { 0 } else { BOL_AT_OFFSET })
                | if nonempty { NONEMPTY_AT_OFFSET } else { 0 };
            match re.find_opts(input, base, options) {
                None => break,
                Some(m) => {
                    let (start, end) = m.groups[0].unwrap();
                    if !output.is_empty() || nonempty {
                        output.push(b';');
                    }
                    output.extend_from_slice(&input[start..end]);
                    base = end;
                    if start == input.len() {
                        break;
                    }
                    nonempty = start == end;
                }
            }
        }
        output
    }

    fn corpus() {
        let json: serde_json::Value =
            serde_json::from_str(include_str!("../tests/data/regex_corpus.json"))
                .expect("corpus JSON parses");
        let cases = json["cases"].as_array().expect("cases array");
        assert!(cases.len() >= 150, "corpus must have >= 150 cases");

        for case in cases {
            let pattern = unhex(case["p"].as_str().unwrap());
            let input = unhex(case["i"].as_str().unwrap());
            // All corpus entries are valid UTF-8 so `find(&str)` applies.
            let pattern = String::from_utf8(pattern).expect("pattern is UTF-8");
            let input = String::from_utf8(input).expect("input is UTF-8");
            let label = format!("pattern {pattern:?} input {input:?}");

            match Regex::new(&pattern) {
                Err(e) => assert!(
                    case["err"].as_bool().unwrap(),
                    "{label}: unexpected compile failure: {e}"
                ),
                Ok(re) => {
                    assert!(
                        !case["err"].as_bool().unwrap(),
                        "{label}: oracle rejected but we compiled"
                    );
                    let hit = case["hit"].as_bool().unwrap();
                    match re.find(&input) {
                        None => assert!(!hit, "{label}: oracle matched but we did not"),
                        Some(m) => {
                            assert!(hit, "{label}: oracle missed but we matched {m:?}");
                            let g = case["g"].as_array().unwrap();
                            for (n, want) in g.iter().enumerate() {
                                let want = unhex(want.as_str().unwrap());
                                let got: &[u8] =
                                    m.groups[n].map_or(&[], |(s, e)| &input.as_bytes()[s..e]);
                                assert_eq!(
                                    got,
                                    want.as_slice(),
                                    "{label}: group {n} mismatch (match {m:?})"
                                );
                            }
                        }
                    }
                    for (key, new_policy) in [("mo", false), ("mn", true)] {
                        let want = unhex(case[key].as_str().unwrap());
                        let got = matchall(&re, input.as_bytes(), new_policy);
                        assert_eq!(got, want, "{label}: matchall {key} mismatch");
                    }
                }
            }
        }
    }

    #[test]
    fn corpus_matches_oracle() {
        // Deep backtracking recursion gets a generous stack.
        std::thread::Builder::new()
            .name("corpus".into())
            .stack_size(256 << 20)
            .spawn(corpus)
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn anchors_and_offsets() {
        let re = Regex::new("^a").unwrap();
        // CMP0186 NEW semantics: ^ only at absolute position 0.
        assert!(re.find_at("aabaa", 1, true).is_none());
        // OLD semantics: ^ matches at the search offset.
        let m = re.find_at("aabaa", 1, false).unwrap();
        assert_eq!(m.groups[0], Some((1, 2)));
        assert_eq!(re.find("aabaa").unwrap().groups[0], Some((0, 1)));
    }

    #[test]
    fn empty_and_groups() {
        let re = Regex::new("(a*)(a*)").unwrap();
        let m = re.find("aa").unwrap();
        assert_eq!(m.groups[0], Some((0, 2)));
        assert_eq!(m.groups[1], Some((0, 2)));
        assert_eq!(m.groups[2], Some((2, 2)));

        let re = Regex::new("(x)*y").unwrap();
        let m = re.find("y").unwrap();
        assert_eq!(m.groups[0], Some((0, 1)));
        assert_eq!(m.groups[1], None);

        // Leftmost-first alternation.
        let re = Regex::new("(a|ab)(c|bcd)").unwrap();
        let m = re.find("abcd").unwrap();
        assert_eq!(m.groups[0], Some((0, 4)));
        assert_eq!(m.groups[1], Some((0, 1)));
        assert_eq!(m.groups[2], Some((1, 4)));
    }

    #[test]
    fn compile_errors() {
        for pat in ["(", ")", "a**", "*a", "[a", "[z-a]", "a\\", "a|*"] {
            assert!(Regex::new(pat).is_err(), "{pat:?} should not compile");
        }
        for pat in ["()", "^?", "$?", "(a*)?", "[]]", "a|"] {
            assert!(Regex::new(pat).is_ok(), "{pat:?} should compile");
        }
    }
}
