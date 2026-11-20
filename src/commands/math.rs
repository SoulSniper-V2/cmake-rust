//! `math()` command: port of `cmMathCommand.cxx`, `cmExprLexer.in.l` and the
//! bison-generated `cmExprParser.cxx` (tables included verbatim).

use crate::interp::{CmdResult, Interp, err};
use crate::messages::MessageType;

pub fn register(interp: &mut Interp) {
    interp.register("math", math_command);
}

// ------------------------------------------------------------------
// Expression lexer (cmExprLexer.in.l)
// ------------------------------------------------------------------

const YYEOF: i32 = 0;
const EXP_PLUS: i32 = 3;
const EXP_MINUS: i32 = 4;
const EXP_TIMES: i32 = 5;
const EXP_DIVIDE: i32 = 6;
const EXP_MOD: i32 = 7;
const EXP_SHIFTLEFT: i32 = 8;
const EXP_SHIFTRIGHT: i32 = 9;
const EXP_OPENPARENT: i32 = 10;
const EXP_CLOSEPARENT: i32 = 11;
const EXP_OR: i32 = 12;
const EXP_AND: i32 = 13;
const EXP_XOR: i32 = 14;
const EXP_NOT: i32 = 15;
const EXP_NUMBER: i32 = 16;
const YYEMPTY: i32 = -2;

const YYTNAME: [&str; 27] = [
    "end of file",
    "error",
    "invalid token",
    "exp_PLUS",
    "exp_MINUS",
    "exp_TIMES",
    "exp_DIVIDE",
    "exp_MOD",
    "exp_SHIFTLEFT",
    "exp_SHIFTRIGHT",
    "exp_OPENPARENT",
    "exp_CLOSEPARENT",
    "exp_OR",
    "exp_AND",
    "exp_XOR",
    "exp_NOT",
    "exp_NUMBER",
    "$accept",
    "start",
    "exp",
    "bitwiseor",
    "bitwisexor",
    "bitwiseand",
    "shift",
    "term",
    "unary",
    "factor",
];

struct Lexer<'a> {
    input: &'a [u8],
    /// Scan cursor (flex pushes its lookahead back here).
    pos: usize,
    /// `cmExprParserHelper::InputBufferPos`: bytes pulled from the input,
    /// including the lookahead flex reads to terminate extendable tokens.
    pulled: usize,
    warnings: String,
    /// Thrown `std::out_of_range` from `std::stoll`.
    out_of_range: bool,
}

impl<'a> Lexer<'a> {
    fn pull(&mut self, upto: usize) {
        self.pulled = self.pulled.max(upto.min(self.input.len()));
    }

    fn bump(&mut self, n: usize) {
        self.pos += n;
        self.pull(self.pos);
    }

    fn unexpected_char(&mut self, c: u8) {
        self.warnings.push_str(&format!(
            "Unexpected character in expression at position {}: {}\n",
            self.pulled, c as char
        ));
    }

    /// Returns (token, value) where value is set for exp_NUMBER.
    fn next(&mut self) -> (i32, i64) {
        let bytes = self.input;
        loop {
            self.pull(self.pos);
            if self.pos >= bytes.len() {
                return (YYEOF, 0);
            }
            let c = bytes[self.pos];
            match c {
                b' ' | b'\t' | b'\n' => {
                    self.bump(1);
                    continue;
                }
                b'0'..=b'9' => {
                    let start = self.pos;
                    // Longest match: prefer the hex rule when it applies.
                    if c == b'0'
                        && matches!(bytes.get(self.pos + 1), Some(b'x') | Some(b'X'))
                        && bytes
                            .get(self.pos + 2)
                            .is_some_and(|b| b.is_ascii_hexdigit())
                    {
                        self.pull(self.pos + 3);
                        self.pos += 2;
                        let hstart = self.pos;
                        while self.pos < bytes.len() && bytes[self.pos].is_ascii_hexdigit() {
                            self.pos += 1;
                        }
                        self.pull(self.pos + 1);
                        let text = std::str::from_utf8(&bytes[hstart..self.pos]).unwrap();
                        match i64::from_str_radix(text, 16) {
                            Ok(v) => return (EXP_NUMBER, v),
                            Err(_) => {
                                self.out_of_range = true;
                                return (EXP_NUMBER, 0);
                            }
                        }
                    }
                    if c == b'0'
                        && matches!(bytes.get(self.pos + 1), Some(b'x') | Some(b'X'))
                    {
                        self.pull(self.pos + 3);
                    }
                    while self.pos < bytes.len() && bytes[self.pos].is_ascii_digit() {
                        self.pos += 1;
                    }
                    self.pull(self.pos + 1);
                    let text = std::str::from_utf8(&bytes[start..self.pos]).unwrap();
                    match text.parse::<i64>() {
                        Ok(v) => return (EXP_NUMBER, v),
                        Err(_) => {
                            self.out_of_range = true;
                            return (EXP_NUMBER, 0);
                        }
                    }
                }
                b'+' => {
                    self.bump(1);
                    return (EXP_PLUS, 0);
                }
                b'-' => {
                    self.bump(1);
                    return (EXP_MINUS, 0);
                }
                b'*' => {
                    self.bump(1);
                    return (EXP_TIMES, 0);
                }
                b'/' => {
                    self.bump(1);
                    return (EXP_DIVIDE, 0);
                }
                b'%' => {
                    self.bump(1);
                    return (EXP_MOD, 0);
                }
                b'|' => {
                    self.bump(1);
                    return (EXP_OR, 0);
                }
                b'&' => {
                    self.bump(1);
                    return (EXP_AND, 0);
                }
                b'^' => {
                    self.bump(1);
                    return (EXP_XOR, 0);
                }
                b'~' => {
                    self.bump(1);
                    return (EXP_NOT, 0);
                }
                b'<' => {
                    self.pull(self.pos + 2);
                    if bytes.get(self.pos + 1) == Some(&b'<') {
                        self.bump(2);
                        return (EXP_SHIFTLEFT, 0);
                    }
                    self.bump(1);
                    self.unexpected_char(c);
                }
                b'>' => {
                    self.pull(self.pos + 2);
                    if bytes.get(self.pos + 1) == Some(&b'>') {
                        self.bump(2);
                        return (EXP_SHIFTRIGHT, 0);
                    }
                    self.bump(1);
                    self.unexpected_char(c);
                }
                b'(' => {
                    self.bump(1);
                    return (EXP_OPENPARENT, 0);
                }
                b')' => {
                    self.bump(1);
                    return (EXP_CLOSEPARENT, 0);
                }
                other => {
                    self.bump(1);
                    self.unexpected_char(other);
                }
            }
        }
    }
}

// ------------------------------------------------------------------
// bison tables from cmExprParser.cxx (verbatim)
// ------------------------------------------------------------------

const YYPACT: [i32; 41] = [
    1, 1, 1, 1, 1, -11, 6, -10, -4, 9, 4, 11, 2, -11, -11, -11, -11, 7, -11, -11, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, -11, -4, 9, 4, 11, 11, 2, 2, -11, -11, -11,
];

const YYDEFACT: [i32; 41] = [
    0, 0, 0, 0, 0, 23, 0, 2, 3, 5, 7, 9, 12, 15, 19, 20, 21, 0, 22, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 24, 4, 6, 8, 10, 11, 13, 14, 16, 17, 18,
];

const YYPGOTO: [i32; 10] = [-11, -11, 22, 10, 8, 12, -3, -2, -1, -11];

const YYDEFGOTO: [i32; 10] = [0, 6, 7, 8, 9, 10, 11, 12, 13, 14];

const YYTABLE: [i32; 35] = [
    15, 16, 20, 18, 1, 2, 19, 27, 28, 29, 21, 3, 23, 24, 25, 26, 4, 5, 30, 20, 34, 35, 22, 36, 37,
    17, 38, 39, 40, 32, 31, 0, 0, 0, 33,
];

const YYCHECK: [i32; 35] = [
    1, 2, 12, 4, 3, 4, 0, 5, 6, 7, 14, 10, 8, 9, 3, 4, 15, 16, 11, 12, 23, 24, 13, 25, 26, 3, 27,
    28, 29, 21, 20, -1, -1, -1, 22,
];

const YYR1: [i32; 25] = [
    0, 17, 18, 19, 19, 20, 20, 21, 21, 22, 22, 22, 23, 23, 23, 24, 24, 24, 24, 25, 25, 25, 25, 26,
    26,
];

const YYR2: [i32; 25] = [
    0, 2, 1, 1, 3, 1, 3, 1, 3, 1, 3, 3, 1, 3, 3, 1, 3, 3, 3, 1, 2, 2, 2, 1, 3,
];

const YYPACT_NINF: i32 = -11;
const YYTABLE_NINF: i32 = -1;
const YYLAST: i32 = 34;
const YYNTOKENS: i32 = 17;
const YYFINAL: i32 = 19;

enum ExprError {
    /// `cannot parse the expression` — syntax error string.
    Parse(String),
    /// `cannot evaluate the expression` — runtime error string.
    Eval(String),
    /// `std::out_of_range` — "a numeric value is out of range."
    OutOfRange,
}

/// `yypcontext_expected_tokens`.
fn expected_tokens(state: i32, out: &mut Vec<i32>) {
    let yyn = YYPACT[state as usize];
    if yyn != YYPACT_NINF {
        let yyxbegin = if yyn < 0 { -yyn } else { 0 };
        let yychecklim = YYLAST - yyn + 1;
        let yyxend = if yychecklim < YYNTOKENS {
            yychecklim
        } else {
            YYNTOKENS
        };
        for yyx in yyxbegin..yyxend {
            // `yytable_value_is_error` is hardwired to 0 in this grammar, so
            // every symbol matching yycheck counts as expected.
            if YYCHECK[(yyx + yyn) as usize] == yyx && yyx != 1 {
                out.push(yyx);
            }
        }
    }
}

/// `yysyntax_error`. The oracle binary was generated with an older bison
/// skeleton: token names are unquoted and the "expecting" list is omitted
/// entirely when five or more tokens would be listed
/// (`YYERROR_VERBOSE_ARGS_MAXIMUM` includes the unexpected token itself).
fn syntax_error(state: i32, token: i32) -> String {
    let mut yyarg: Vec<i32> = Vec::new();
    if token != YYEMPTY {
        yyarg.push(token);
        expected_tokens(state, &mut yyarg);
        if yyarg.len() > 5 {
            yyarg.truncate(1);
        }
    }
    let names: Vec<&str> = yyarg.iter().map(|&t| YYTNAME[t as usize]).collect();
    match names.len() {
        0 => "syntax error".to_owned(),
        1 => format!("syntax error, unexpected {}", names[0]),
        2 => format!("syntax error, unexpected {}, expecting {}", names[0], names[1]),
        3 => format!(
            "syntax error, unexpected {}, expecting {} or {}",
            names[0], names[1], names[2]
        ),
        4 => format!(
            "syntax error, unexpected {}, expecting {} or {} or {}",
            names[0], names[1], names[2], names[3]
        ),
        _ => format!(
            "syntax error, unexpected {}, expecting {} or {} or {} or {}",
            names[0], names[1], names[2], names[3], names[4]
        ),
    }
}

/// SIGFPE like C++ UB on integer division overflow / mod by zero.
fn sigfpe() -> ! {
    unsafe { libc::raise(libc::SIGFPE) };
    std::process::exit(128 + libc::SIGFPE);
}

/// Semantic action for rule `r` applied to popped values (non-error rules).
/// Rules: 4 = |, 6 = ^, 8 = &, 10 = <<, 11 = >>, 13 = +, 14 = -, 16 = *,
/// 20 = +unary, 21 = -unary, 22 = ~, 24 = parens; 17 = / and 18 = % are
/// handled separately for their error semantics.
fn reduce(r: i32, rhs: &[i64]) -> i64 {
    match r {
        4 => rhs[0] | rhs[2],
        6 => rhs[0] ^ rhs[2],
        8 => rhs[0] & rhs[2],
        10 => rhs[0].wrapping_shl(rhs[2] as u32),
        11 => rhs[0].wrapping_shr(rhs[2] as u32),
        13 => rhs[0].wrapping_add(rhs[2]),
        14 => rhs[0].wrapping_sub(rhs[2]),
        16 => rhs[0].wrapping_mul(rhs[2]),
        20 => rhs[1],
        21 => rhs[1].wrapping_neg(),
        22 => !rhs[1],
        24 => rhs[1],
        _ => rhs[0],
    }
}

struct Parser<'a> {
    lexer: Lexer<'a>,
    stack: Vec<i32>,
    values: Vec<i64>,
    token: i32,
    token_value: i64,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Parser<'a> {
        Parser {
            lexer: Lexer {
                input: input.as_bytes(),
                pos: 0,
                pulled: 0,
                warnings: String::new(),
                out_of_range: false,
            },
            stack: vec![0],
            values: Vec::new(),
            token: YYEMPTY,
            token_value: 0,
        }
    }

    fn read_token(&mut self) {
        let (tok, val) = self.lexer.next();
        self.token = tok;
        self.token_value = val;
    }

    /// `cmExpr_yyparse` — returns result value or error kind.
    fn parse(&mut self) -> Result<i64, ExprError> {
        loop {
            let yystate = *self.stack.last().unwrap();
            if yystate == YYFINAL {
                return Ok(self.values[0]);
            }
            let mut yyn = YYPACT[yystate as usize];
            if yyn != YYPACT_NINF {
                if self.token == YYEMPTY {
                    self.read_token();
                    if self.lexer.out_of_range {
                        return Err(ExprError::OutOfRange);
                    }
                }
                yyn += self.token;
                if !(0..=YYLAST).contains(&yyn) || YYCHECK[yyn as usize] != self.token {
                    yyn = YYDEFACT[yystate as usize];
                } else {
                    yyn = YYTABLE[yyn as usize];
                    if yyn <= 0 {
                        if yyn == YYTABLE_NINF {
                            return Err(ExprError::Parse(syntax_error(yystate, self.token)));
                        }
                        self.reduce_rule(-yyn)?;
                        continue;
                    }
                    // Shift.
                    self.stack.push(yyn);
                    self.values.push(self.token_value);
                    self.token = YYEMPTY;
                    continue;
                }
            } else {
                yyn = YYDEFACT[yystate as usize];
            }
            if yyn == 0 {
                return Err(ExprError::Parse(syntax_error(yystate, self.token)));
            }
            self.reduce_rule(yyn)?;
        }
    }

    fn reduce_rule(&mut self, yyn: i32) -> Result<(), ExprError> {
        let yylen = YYR2[yyn as usize] as usize;
        let start = self.values.len() - yylen;
        let rhs: Vec<i64> = self.values[start..].to_vec();
        let value = match yyn {
            17 => {
                if rhs[2] == 0 {
                    return Err(ExprError::Eval("divide by zero".to_owned()));
                }
                if rhs[0] == i64::MIN && rhs[2] == -1 {
                    sigfpe();
                }
                rhs[0].wrapping_div(rhs[2])
            }
            18 => {
                if rhs[2] == 0 || (rhs[0] == i64::MIN && rhs[2] == -1) {
                    sigfpe();
                }
                rhs[0].wrapping_rem(rhs[2])
            }
            _ => reduce(yyn, &rhs),
        };
        self.values.truncate(start);
        self.values.push(value);
        self.stack.truncate(self.stack.len() - yylen);
        // goto
        let yylhs = YYR1[yyn as usize] - YYNTOKENS;
        let top = *self.stack.last().unwrap();
        let yyi = YYPGOTO[yylhs as usize] + top;
        let next = if (0..=YYLAST).contains(&yyi) && YYCHECK[yyi as usize] == top {
            YYTABLE[yyi as usize]
        } else {
            YYDEFGOTO[yylhs as usize]
        };
        self.stack.push(next);
        Ok(())
    }
}

fn parse_expression(input: &str) -> Result<(i64, String), String> {
    let mut parser = Parser::new(input);
    match parser.parse() {
        Ok(value) => Ok((value, parser.lexer.warnings)),
        Err(ExprError::OutOfRange) => Err(format!(
            "cannot evaluate the expression: \"{input}\": a numeric value is out of range."
        )),
        Err(ExprError::Eval(msg)) => Err(format!(
            "cannot evaluate the expression: \"{input}\": {msg}."
        )),
        Err(ExprError::Parse(msg)) => {
            // Error() appends " (InputBufferPos)".
            let pos = parser.lexer.pulled;
            Err(format!(
                "cannot parse the expression: \"{input}\": {msg} ({pos})."
            ))
        }
    }
}

// ------------------------------------------------------------------
// command
// ------------------------------------------------------------------

fn math_command(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.is_empty() {
        return err("must be called with at least one argument.");
    }
    if args[0] != "EXPR" {
        return err(format!("does not recognize sub-command {}", args[0]));
    }
    if args.len() != 3 && args.len() != 5 {
        return err("EXPR called with incorrect arguments.");
    }

    enum Format {
        Decimal,
        Hexadecimal,
    }
    let mut output_format = Format::Decimal;
    const HINT: &str = "sub-command EXPR ";
    if args.len() > 3 {
        let option = &args[3];
        if option == "OUTPUT_FORMAT" {
            if args.len() > 4 {
                output_format = match args[4].as_str() {
                    "DECIMAL" => Format::Decimal,
                    "HEXADECIMAL" => Format::Hexadecimal,
                    argument => {
                        return err(format!(
                            "{HINT}value \"{argument}\" for option \"{option}\" is invalid."
                        ));
                    }
                };
            } else {
                return err(format!("{HINT}missing argument for option \"{option}\"."));
            }
        } else {
            return err(format!("{HINT}option \"{option}\" is unknown."));
        }
    }

    let output_variable = &args[1];
    let expression = &args[2];
    interp.set_def(output_variable.clone(), "ERROR");

    let (value, warning) = match parse_expression(expression) {
        Ok(v) => v,
        Err(e) => return err(e),
    };

    let buffer = match output_format {
        Format::Hexadecimal => format!("0x{:x}", value as u64),
        Format::Decimal => format!("{value}"),
    };

    if !warning.is_empty() {
        interp.issue(MessageType::AuthorWarning, &warning);
    }
    interp.set_def(output_variable.clone(), buffer);
    Ok(())
}
