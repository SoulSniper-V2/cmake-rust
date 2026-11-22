//! Faithful port of `Utilities/cmllpkgc/llpkgc.c` + `llpkgc__internal.c`
//! (the generated pkg-config state machine) and `Source/cmPkgConfigParser.cxx`.
//!
//! Only the single-buffer use case is supported (`parse(buf)` then `finish()`),
//! which is all `cmPkgConfigParser::Finish(char*, size_t)` needs.  State is
//! kept resumable anyway to mirror the original control flow.

/// `llpkgc_errno_t`.
pub const PCE_OK: i32 = 0;
pub const PCE_UNFINISHED: i32 = 4;

/// `cmPkgConfigValueElement`.
#[derive(Clone, Debug)]
pub struct ValueElement {
    pub is_variable: bool,
    pub data: String,
}

/// `cmPkgConfigEntry`.
#[derive(Clone, Debug)]
pub struct Entry {
    /// true: `key = value` variable; false: `key: value` keyword.
    pub is_variable: bool,
    pub key: String,
    pub val: Vec<ValueElement>,
}

/// Which span callback `_span_cb0` currently designates.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SpanCb {
    Key,
    ValLit,
    ValVar,
}

/// `llparse_state_t`: resumable machine states plus the transient goto labels
/// of `llpkgc__internal__run` (span ends, callback invokes, escaped updates).
#[derive(Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
enum S {
    Error,
    Comment,
    InvokeVallitComplete,
    InvokeVallitComplete1,
    LiteralSkipDollar,
    InvokeVallitComplete2,
    MaybeCr,
    SkipEscapedLf,
    InvokeVallitComplete3,
    MaybeLf,
    SkipEscapedCr,
    InvokeVallitComplete4,
    LiteralSkipHash,
    SpanStartVallitSpan1,
    InvokeVallitComplete5,
    MaybeEscaped,
    Literal,
    VariableSkipDollar,
    SpanStartVallitSpan,
    SpanStartVallitSpan2,
    VariableClose,
    VariableSkipCurly,
    InvokeValvarComplete,
    Variable,
    SpanStartValvarSpan,
    MaybeVariable,
    ExpectValue,
    ExpectSep,
    Key,
    SpanStartKeySpan,
    LineStart,
    // Transient labels that live after the big switch in the generated code.
    InvokeValueComplete,
    InvokeValueComplete1,
    InvokeValueComplete2,
    SpanEndVallitSpan,
    SpanEndVallitSpan1,
    SpanEndVallitSpan2,
    SpanEndVallitSpan3,
    SpanEndVallitSpan4,
    SpanEndVallitSpan5,
    UpdateEscaped,
    UpdateEscaped1,
    UpdateEscaped2,
    SpanEndValvarSpan,
    SpanEndKeySpan,
    InvokeKeywordComplete,
    InvokeVariableComplete,
    InvokeLineBegin,
}

/// `cmPkgConfigParser` (`llpkgc_t` + the callback bookkeeping in the .cxx).
pub struct Parser {
    buf: Vec<u8>,

    // llpkgc__internal_t
    current: S,
    error: i32,
    #[allow(dead_code)]
    reason: &'static str,
    #[allow(dead_code)]
    error_pos: usize,
    span_pos0: Option<usize>,
    span_cb0: SpanCb,
    unfinished_: u8,
    escaped_: bool,

    // cmPkgConfigParser: span origin/length of the piece being assembled and
    // which handler flavor each data callback currently points at
    // (`OnKeyTr` vs `OnSpanNextTr` etc.).
    ptr_: usize,
    len_: usize,
    on_key_next: bool,
    on_vallit_next: bool,
    on_valvar_next: bool,

    data: Vec<Entry>,
}

impl Parser {
    /// `cmPkgConfigParser::cmPkgConfigParser` (`llpkgc_init`).
    pub fn new() -> Self {
        Parser {
            buf: Vec::new(),
            current: S::LineStart,
            error: 0,
            reason: "",
            error_pos: 0,
            span_pos0: None,
            span_cb0: SpanCb::Key,
            unfinished_: 0,
            escaped_: false,
            ptr_: 0,
            len_: 0,
            on_key_next: false,
            on_vallit_next: false,
            on_valvar_next: false,
            data: Vec::new(),
        }
    }

    pub fn data(&self) -> &[Entry] {
        &self.data
    }

    /// `cmPkgConfigParser::Finish(buf, len)` = `Parse` + `llpkgc_finish`.
    pub fn finish_with(&mut self, buf: &[u8]) -> i32 {
        self.buf = buf.to_vec();
        self.execute();
        self.finish()
    }

    /// `llpkgc_execute`.
    fn execute(&mut self) -> i32 {
        if self.error != 0 {
            return self.error;
        }
        // Restart spans: continued data begins at the new buffer start.
        if self.span_pos0.is_some() {
            self.span_pos0 = Some(0);
        }
        let next = self.run();
        if next == S::Error {
            return self.error;
        }
        self.current = next;
        // Execute pending spans against the end of the buffer.
        if let Some(start) = self.span_pos0 {
            let endp = self.buf.len();
            match self.span_cb0 {
                SpanCb::Key => self.key_span(start, endp),
                SpanCb::ValLit => self.vallit_span(start, endp),
                SpanCb::ValVar => self.valvar_span(start, endp),
            }
        }
        0
    }

    /// `llpkgc_finish`.
    pub fn finish(&mut self) -> i32 {
        if self.error != 0 {
            return self.error;
        }
        match self.unfinished_ {
            1 => {
                self.reason = "Invalid EOF state";
                self.error = PCE_UNFINISHED;
                PCE_UNFINISHED
            }
            2 => {
                self.on_value_literal_complete();
                // on_value_complete / on_pkgc_complete are unset (return 0).
                0
            }
            3 => {
                // on_value_complete / on_pkgc_complete are unset (return 0).
                0
            }
            _ => 0,
        }
    }

    // ------------------------------------------------------------------
    // llpkgc.c callback trampolines (unfinished_/escaped_ bookkeeping).

    fn line_begin(&mut self) {
        self.unfinished_ = 1;
        // on_line_begin is unset.
    }

    fn key_span(&mut self, p: usize, endp: usize) {
        self.on_key(p, endp - p);
    }

    fn keyword_complete(&mut self) {
        self.unfinished_ = 3;
        self.on_keyword_complete();
    }

    fn variable_complete(&mut self) {
        self.unfinished_ = 3;
        self.on_variable_complete();
    }

    fn vallit_span(&mut self, p: usize, mut endp: usize) {
        if self.escaped_ {
            endp -= 1;
            self.escaped_ = false;
        }
        self.unfinished_ = 2;
        self.on_value_literal(p, endp - p);
    }

    fn vallit_complete(&mut self) {
        self.unfinished_ = 3;
        self.on_value_literal_complete();
    }

    fn valvar_span(&mut self, p: usize, endp: usize) {
        self.unfinished_ = 1;
        self.on_value_variable(p, endp - p);
    }

    fn valvar_complete(&mut self) {
        self.unfinished_ = 3;
        self.on_value_variable_complete();
    }

    fn value_complete(&mut self) {
        self.unfinished_ = 0;
        // on_value_complete is unset.
    }

    // ------------------------------------------------------------------
    // cmPkgConfigParser user callbacks.

    fn text(&self, at: usize, len: usize) -> String {
        String::from_utf8_lossy(&self.buf[at..at + len]).into_owned()
    }

    /// `OnSpanNext`: accumulate continuation span lengths.
    fn on_key(&mut self, at: usize, len: usize) {
        if self.on_key_next {
            self.len_ += len;
        } else {
            // `OnKey`: first span of a key.
            self.ptr_ = at;
            self.len_ = len;
            self.on_key_next = true;
        }
    }

    fn on_keyword_complete(&mut self) {
        let key = self.text(self.ptr_, self.len_);
        self.data.push(Entry {
            is_variable: false,
            key,
            val: Vec::new(),
        });
        self.on_key_next = false;
    }

    fn on_variable_complete(&mut self) {
        let key = self.text(self.ptr_, self.len_);
        self.data.push(Entry {
            is_variable: true,
            key,
            val: Vec::new(),
        });
        self.on_key_next = false;
    }

    fn on_value_literal(&mut self, at: usize, len: usize) {
        if self.on_vallit_next {
            self.len_ += len;
        } else {
            self.ptr_ = at;
            self.len_ = len;
            self.on_vallit_next = true;
        }
    }

    fn on_value_literal_complete(&mut self) {
        self.on_vallit_next = false;
        if self.len_ != 0 {
            let data = self.text(self.ptr_, self.len_);
            if let Some(back) = self.data.last_mut() {
                back.val.push(ValueElement {
                    is_variable: false,
                    data,
                });
            }
        }
    }

    fn on_value_variable(&mut self, at: usize, len: usize) {
        if self.on_valvar_next {
            self.len_ += len;
        } else {
            self.ptr_ = at;
            self.len_ = len;
            self.on_valvar_next = true;
        }
    }

    fn on_value_variable_complete(&mut self) {
        self.on_valvar_next = false;
        let data = self.text(self.ptr_, self.len_);
        if let Some(back) = self.data.last_mut() {
            back.val.push(ValueElement {
                is_variable: true,
                data,
            });
        }
    }

    // ------------------------------------------------------------------
    // llpkgc__internal.c internals.

    fn fail(&mut self, code: i32, reason: &'static str, p: usize) -> S {
        self.error = code;
        self.reason = reason;
        self.error_pos = p;
        self.current = S::Error;
        S::Error
    }

    /// `llpkgc__internal__run`.  Returns the resumable state to store, or
    /// `S::Error` (with `self.error` set by an error label).
    fn run(&mut self) -> S {
        let endp = self.buf.len();
        let mut p = 0usize;
        let mut s = self.current;
        loop {
            s = match s {
                S::Comment => {
                    if p == endp {
                        return S::Comment;
                    }
                    match self.buf[p] {
                        10 | 13 => {
                            p += 1;
                            S::LineStart
                        }
                        _ => {
                            p += 1;
                            S::Comment
                        }
                    }
                }
                S::InvokeVallitComplete => {
                    self.vallit_complete();
                    S::InvokeValueComplete1
                }
                S::InvokeVallitComplete1 => {
                    self.vallit_complete();
                    S::InvokeValueComplete2
                }
                S::LiteralSkipDollar => {
                    if p == endp {
                        return S::LiteralSkipDollar;
                    }
                    p += 1;
                    S::MaybeVariable
                }
                S::InvokeVallitComplete2 => {
                    self.vallit_complete();
                    S::LiteralSkipDollar
                }
                S::MaybeCr => {
                    if p == endp {
                        return S::MaybeCr;
                    }
                    if self.buf[p] == 13 {
                        p += 1;
                    }
                    S::ExpectValue
                }
                S::SkipEscapedLf => {
                    if p == endp {
                        return S::SkipEscapedLf;
                    }
                    p += 1;
                    S::MaybeCr
                }
                S::InvokeVallitComplete3 => {
                    self.vallit_complete();
                    S::SkipEscapedLf
                }
                S::MaybeLf => {
                    if p == endp {
                        return S::MaybeLf;
                    }
                    if self.buf[p] == 10 {
                        p += 1;
                    }
                    S::ExpectValue
                }
                S::SkipEscapedCr => {
                    if p == endp {
                        return S::SkipEscapedCr;
                    }
                    p += 1;
                    S::MaybeLf
                }
                S::InvokeVallitComplete4 => {
                    self.vallit_complete();
                    S::SkipEscapedCr
                }
                S::LiteralSkipHash => {
                    if p == endp {
                        return S::LiteralSkipHash;
                    }
                    p += 1;
                    S::Literal
                }
                S::SpanStartVallitSpan1 => {
                    if p == endp {
                        return S::SpanStartVallitSpan1;
                    }
                    self.span_pos0 = Some(p);
                    self.span_cb0 = SpanCb::ValLit;
                    S::LiteralSkipHash
                }
                S::InvokeVallitComplete5 => {
                    self.vallit_complete();
                    S::SpanStartVallitSpan1
                }
                S::MaybeEscaped => {
                    if p == endp {
                        return S::MaybeEscaped;
                    }
                    match self.buf[p] {
                        10 => S::UpdateEscaped,
                        13 => S::UpdateEscaped1,
                        b'#' => S::UpdateEscaped2,
                        _ => S::Literal,
                    }
                }
                S::Literal => {
                    if p == endp {
                        return S::Literal;
                    }
                    match self.buf[p] {
                        10 | 13 => S::SpanEndVallitSpan,
                        b'#' => S::SpanEndVallitSpan1,
                        b'$' => S::SpanEndVallitSpan2,
                        92 => {
                            p += 1;
                            S::MaybeEscaped
                        }
                        _ => {
                            p += 1;
                            S::Literal
                        }
                    }
                }
                S::VariableSkipDollar => {
                    if p == endp {
                        return S::VariableSkipDollar;
                    }
                    p += 1;
                    S::Literal
                }
                S::SpanStartVallitSpan => {
                    if p == endp {
                        return S::SpanStartVallitSpan;
                    }
                    self.span_pos0 = Some(p);
                    self.span_cb0 = SpanCb::ValLit;
                    S::VariableSkipDollar
                }
                S::SpanStartVallitSpan2 => {
                    if p == endp {
                        return S::SpanStartVallitSpan2;
                    }
                    self.span_pos0 = Some(p);
                    self.span_cb0 = SpanCb::ValLit;
                    S::Literal
                }
                S::VariableClose => {
                    if p == endp {
                        return S::VariableClose;
                    }
                    match self.buf[p] {
                        10 | 13 | b'#' => S::InvokeValueComplete,
                        b'$' => {
                            p += 1;
                            S::MaybeVariable
                        }
                        _ => S::SpanStartVallitSpan2,
                    }
                }
                S::VariableSkipCurly => {
                    if p == endp {
                        return S::VariableSkipCurly;
                    }
                    p += 1;
                    S::VariableClose
                }
                S::InvokeValvarComplete => {
                    self.valvar_complete();
                    S::VariableSkipCurly
                }
                S::Variable => {
                    if p == endp {
                        return S::Variable;
                    }
                    match self.buf[p] {
                        b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z' | b'_' | b'.' => {
                            p += 1;
                            S::Variable
                        }
                        b'}' => S::SpanEndValvarSpan,
                        _ => return self.fail(0xd, "Invalid variable character", p),
                    }
                }
                S::SpanStartValvarSpan => {
                    if p == endp {
                        return S::SpanStartValvarSpan;
                    }
                    self.span_pos0 = Some(p);
                    self.span_cb0 = SpanCb::ValVar;
                    S::Variable
                }
                S::MaybeVariable => {
                    if p == endp {
                        return S::MaybeVariable;
                    }
                    match self.buf[p] {
                        b'$' => S::SpanStartVallitSpan,
                        b'{' => {
                            p += 1;
                            S::SpanStartValvarSpan
                        }
                        _ => return self.fail(0x9, "Unexpected `$`", p),
                    }
                }
                S::ExpectValue => {
                    if p == endp {
                        return S::ExpectValue;
                    }
                    match self.buf[p] {
                        9 | b' ' => {
                            p += 1;
                            S::ExpectValue
                        }
                        10 | 13 | b'#' => S::InvokeValueComplete,
                        b'$' => {
                            p += 1;
                            S::MaybeVariable
                        }
                        _ => S::SpanStartVallitSpan2,
                    }
                }
                S::ExpectSep => {
                    if p == endp {
                        return S::ExpectSep;
                    }
                    match self.buf[p] {
                        9 | b' ' => {
                            p += 1;
                            S::ExpectSep
                        }
                        b':' => {
                            p += 1;
                            S::InvokeKeywordComplete
                        }
                        b'=' => {
                            p += 1;
                            S::InvokeVariableComplete
                        }
                        _ => return self.fail(0x7, "Expected seperator", p),
                    }
                }
                S::Key => {
                    if p == endp {
                        return S::Key;
                    }
                    match self.buf[p] {
                        9 | b' ' | b':' | b'=' => S::SpanEndKeySpan,
                        b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z' | b'_' | b'.' => {
                            p += 1;
                            S::Key
                        }
                        _ => return self.fail(0x4, "Invalid key character", p),
                    }
                }
                S::SpanStartKeySpan => {
                    if p == endp {
                        return S::SpanStartKeySpan;
                    }
                    self.span_pos0 = Some(p);
                    self.span_cb0 = SpanCb::Key;
                    S::Key
                }
                S::LineStart => {
                    if p == endp {
                        return S::LineStart;
                    }
                    match self.buf[p] {
                        9 | 10 | 13 | b' ' => {
                            p += 1;
                            S::LineStart
                        }
                        b'#' => {
                            p += 1;
                            S::Comment
                        }
                        b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z' | b'_' | b'.' => {
                            S::InvokeLineBegin
                        }
                        _ => return self.fail(0x2, "Expected key", p),
                    }
                }
                S::InvokeValueComplete | S::InvokeValueComplete1 => {
                    self.value_complete();
                    S::LineStart
                }
                S::InvokeValueComplete2 => {
                    self.value_complete();
                    S::Comment
                }
                S::SpanEndVallitSpan => {
                    self.span_end_vallit(p);
                    S::InvokeVallitComplete
                }
                S::SpanEndVallitSpan1 => {
                    self.span_end_vallit(p);
                    S::InvokeVallitComplete1
                }
                S::SpanEndVallitSpan2 => {
                    self.span_end_vallit(p);
                    S::InvokeVallitComplete2
                }
                S::SpanEndVallitSpan3 => {
                    self.span_end_vallit(p);
                    S::InvokeVallitComplete3
                }
                S::SpanEndVallitSpan4 => {
                    self.span_end_vallit(p);
                    S::InvokeVallitComplete4
                }
                S::SpanEndVallitSpan5 => {
                    self.span_end_vallit(p);
                    S::InvokeVallitComplete5
                }
                S::UpdateEscaped => {
                    self.escaped_ = true;
                    S::SpanEndVallitSpan3
                }
                S::UpdateEscaped1 => {
                    self.escaped_ = true;
                    S::SpanEndVallitSpan4
                }
                S::UpdateEscaped2 => {
                    self.escaped_ = true;
                    S::SpanEndVallitSpan5
                }
                S::SpanEndValvarSpan => {
                    self.span_end_valvar(p);
                    S::InvokeValvarComplete
                }
                S::SpanEndKeySpan => {
                    self.span_end_key(p);
                    S::ExpectSep
                }
                S::InvokeKeywordComplete => {
                    self.keyword_complete();
                    S::ExpectValue
                }
                S::InvokeVariableComplete => {
                    self.variable_complete();
                    S::ExpectValue
                }
                S::InvokeLineBegin => {
                    self.line_begin();
                    S::SpanStartKeySpan
                }
                S::Error => return S::Error,
            };
        }
    }

    /// `s_n_..._span_end_llpkgc__vallit_span*`: flush the pending literal span
    /// and leave `_span_pos0` cleared.
    fn span_end_vallit(&mut self, p: usize) {
        let start = self.span_pos0.take().unwrap_or(0);
        self.vallit_span(start, p);
    }

    /// `s_n_..._span_end_llpkgc__valvar_span`.
    fn span_end_valvar(&mut self, p: usize) {
        let start = self.span_pos0.take().unwrap_or(0);
        self.valvar_span(start, p);
    }

    /// `s_n_..._span_end_llpkgc__key_span`.
    fn span_end_key(&mut self, p: usize) {
        let start = self.span_pos0.take().unwrap_or(0);
        self.key_span(start, p);
    }
}
