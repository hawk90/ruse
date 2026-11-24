//! The expression register (`"=`) evaluator — a CLEARLY-SCOPED arithmetic/string calculator, NOT a
//! Vimscript interpreter. Vim's `"=` evaluates a full Vimscript expression (`:help quote=`); ruse has no
//! Vimscript, so faking one would be dishonest. This module covers the genuinely useful common cases and
//! nothing else:
//!
//! - integer and float arithmetic `+ - * / %` with parentheses, unary `-`/`+`, and correct precedence;
//! - Vim's int-vs-float result rules (`1+1`->`2`, `3/2`->`1` truncated integer division, `3.0/2`->`1.5`,
//!   floats printed in Vim's `%g`-like 6-significant form — all verified against nvim v0.12.4);
//! - number literals as nvim's `eval_number` reads them (verified against nvim v0.12.6): decimal, octal
//!   (`010`, `0o17`; `08`/`018` fall back to decimal), hex (`0x1F`), binary (`0b11`), saturating at
//!   `i64::MAX` like `vim_str2nr`, and floats (`1.5`, `1.5e3`; a fraction is required, so `1e3` is an error);
//! - string literals in single quotes (`''` escapes a quote) or double quotes with nvim's backslash escapes
//!   (`\n \t \r \b \e \f \\ \"`, octal `\101`, `\x41`, `€`, `\U0001F600`, and the `\<Esc>`-style keys
//!   that stand for a character, see [`lex_double_quote`]). A result is bytes, not always UTF-8
//!   (`"\xff"`), and a NUL ends a string literal as it does in nvim;
//! - the `.` (or `..`) string-concatenation operator with number->string coercion (`'n='.5` -> `n=5`,
//!   `'v'.(1+2)` -> `v3`).
//!
//! Explicitly OUT OF SCOPE (each yields [`ExprError`] -> the register is EMPTY, never a crash): variables,
//! `@r`/`&opt`/`$ENV`/`v:` refs, function/method calls, comparisons and logical/ternary/bitwise operators,
//! lists/dicts/blobs, and the `**` power operator. A name reports nvim's `E121`/`E117` (ruse has no
//! variables or functions); the other forms report `E15`.
//!
//! A failure carries nvim's error (see [`ExprError::message`]), found left to right as nvim's recursive
//! descent finds it: the lexer stops at the first text it cannot read and leaves an error token there, so
//! that text is an error only if the parser reaches it as an operand, and `E488` trailing characters if it
//! follows a complete expression (`1'000`).
//!
//! Two DELIBERATE divergences from Vim, chosen so the calculator is intuitive rather than bug-compatible
//! with Vimscript warts:
//! 1. `.` (concat) binds LOOSER than `+ - * / %` here, whereas Vim puts `.` at the additive level. So
//!    `'n='.1+2` is `'n=' . (1+2)` -> `n=3` here (Vim: `('n='.1)+2` with string->number coercion -> `2`).
//!    This module does NOT do string->number coercion in arithmetic at all — an arithmetic operator over a
//!    non-numeric string is a [`ExprError::Type`].
//! 2. Division / modulo by zero is [`ExprError::DivByZero`] (-> empty), not Vim's quirks (`7/0`->i64::MAX,
//!    `5%0`->0, `1.0/0`->inf).

use std::fmt;

/// Why an expression did not evaluate to a value. The `"=` register is then empty, so the paste/insert does
/// nothing, and [`ExprError::message`] is what nvim v0.12.6 shows for the same input (`E15: Invalid
/// expression: "1+"`), which every `"=` entry point reports.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ExprError {
    /// The input was empty or only whitespace (nvim reuses the previous expression and says nothing).
    Empty,
    /// `E15: Invalid expression: "<text>"`: a missing operand, an operator or character that cannot start
    /// one, or a malformed number literal. The text is the whole expression, except for a malformed number,
    /// where it runs from the number to the end (nvim's `eval_number` reports it there: `1+0x` -> `"0x"`).
    Invalid(String),
    /// `E121: Undefined variable: <name>` — ruse has no variables, so every name is undefined.
    UndefinedVariable(String),
    /// `E117: Unknown function: <name>` — a name followed by `(`; ruse has no functions.
    UnknownFunction(String),
    /// `E110: Missing ')'`.
    MissingParen,
    /// `E114: Missing quote: <text>` — an unterminated `"` string, from its opening quote to the end.
    MissingDoubleQuote(String),
    /// `E115: Missing quote: <text>` — an unterminated `'` string, from its opening quote to the end.
    MissingSingleQuote(String),
    /// `E488: Trailing characters: <text>` — input left after a complete expression (`1'000` -> `'000`).
    Trailing(String),
    /// `E804: Cannot use '%' with Float`.
    FloatModulo,
    /// `E1169: Expression too recursive: <text>` — parentheses nest 1000 deep or more; the text follows
    /// the 1000th `(`.
    TooDeep(String),
    /// An arithmetic operator was applied to a string operand (this calculator does not coerce strings to
    /// numbers). A deliberate divergence: nvim coerces and reports nothing, so this has no E-number.
    Type(String),
    /// Integer or float division/modulo by zero (Vim's zero-divisor quirks are intentionally not copied;
    /// no E-number, as nvim reports nothing).
    DivByZero,
    /// An integer computation overflowed `i64` (no E-number, as nvim reports nothing).
    Overflow,
}

impl ExprError {
    /// nvim's error number for this failure (`"E15"`), or `None` where nvim reports nothing: an empty
    /// expression, and the deliberate divergences ([`ExprError::Type`], [`ExprError::DivByZero`],
    /// [`ExprError::Overflow`]), which nvim evaluates to a value instead.
    #[must_use]
    pub fn code(&self) -> Option<&'static str> {
        Some(match self {
            ExprError::Invalid(_) => "E15",
            ExprError::UndefinedVariable(_) => "E121",
            ExprError::UnknownFunction(_) => "E117",
            ExprError::MissingParen => "E110",
            ExprError::MissingDoubleQuote(_) => "E114",
            ExprError::MissingSingleQuote(_) => "E115",
            ExprError::Trailing(_) => "E488",
            ExprError::FloatModulo => "E804",
            ExprError::TooDeep(_) => "E1169",
            ExprError::Empty | ExprError::Type(_) | ExprError::DivByZero | ExprError::Overflow => {
                return None
            }
        })
    }

    /// The message to show for this failure: nvim's exact text where it has one (see [`ExprError::code`]),
    /// a plain unnumbered message for ruse's deliberate divergences, and `None` for an empty expression,
    /// which nvim passes over in silence.
    #[must_use]
    pub fn message(&self) -> Option<String> {
        (*self != ExprError::Empty).then(|| self.to_string())
    }
}

impl fmt::Display for ExprError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExprError::Empty => write!(f, "empty expression"),
            ExprError::Invalid(t) => write!(f, "E15: Invalid expression: \"{t}\""),
            ExprError::UndefinedVariable(n) => write!(f, "E121: Undefined variable: {n}"),
            ExprError::UnknownFunction(n) => write!(f, "E117: Unknown function: {n}"),
            ExprError::MissingParen => write!(f, "E110: Missing ')'"),
            ExprError::MissingDoubleQuote(t) => write!(f, "E114: Missing quote: {t}"),
            ExprError::MissingSingleQuote(t) => write!(f, "E115: Missing quote: {t}"),
            ExprError::Trailing(t) => write!(f, "E488: Trailing characters: {t}"),
            ExprError::FloatModulo => write!(f, "E804: Cannot use '%' with Float"),
            ExprError::TooDeep(t) => write!(f, "E1169: Expression too recursive: {t}"),
            ExprError::Type(m) => write!(f, "Expression type error: {m}"),
            ExprError::DivByZero => write!(f, "Expression error: division by zero"),
            ExprError::Overflow => write!(f, "Expression error: integer overflow"),
        }
    }
}

/// Evaluate `input` and format the result exactly as Vim would `:echo` it (an integer as decimal, a float
/// in Vim's `%g`-like form, a string verbatim). Returns [`ExprError`] for anything unsupported or malformed;
/// callers that want the Vim "empty register on error" behaviour use [`eval_or_empty`].
///
/// # Errors
/// Returns [`ExprError`] when the input is empty, fails to parse, applies an operator to the wrong type, or
/// divides/mods by zero.
pub fn eval(input: &str) -> Result<Vec<u8>, ExprError> {
    if input.trim().is_empty() {
        return Err(ExprError::Empty);
    }
    let tokens = lex(input);
    let mut p = Parser {
        input,
        tokens: &tokens,
        pos: 0,
        depth: 0,
    };
    let value = p.parse_concat()?;
    // Whatever is left after a complete expression, even text that would not lex (`1'000`), is trailing.
    if let Some(&(_, at)) = p.tokens.get(p.pos) {
        return Err(ExprError::Trailing(input[at..].to_string()));
    }
    Ok(value.into_display())
}

/// Evaluate `input`, yielding the formatted result on success and an EMPTY string on any error — the exact
/// degrade Vim applies to a broken `"=` expression (the paste/insert then inserts nothing), without the
/// error message (the editor's expression-register commands call [`eval`] and report the [`ExprError`]).
#[must_use]
pub fn eval_or_empty(input: &str) -> Vec<u8> {
    eval(input).unwrap_or_default()
}

/// A typed intermediate value. The register only ever yields a STRING (its formatted form), but the
/// evaluator carries the int/float/string distinction so arithmetic and `.` apply Vim's type rules.
#[derive(Clone, PartialEq, Debug)]
enum Value {
    Int(i64),
    Float(f64),
    Str(Vec<u8>),
}

impl Value {
    /// The register payload: how Vim `:echo` would render this value.
    fn into_display(self) -> Vec<u8> {
        match self {
            Value::Int(i) => i.to_string().into_bytes(),
            Value::Float(f) => format_vim_float(f).into_bytes(),
            Value::Str(s) => s,
        }
    }

    /// The `.`-concatenation coercion: numbers render to their display form, strings pass through.
    fn coerce_str(self) -> Vec<u8> {
        self.into_display()
    }
}

/// Format a float the way Vim `:echo`s it (verified against nvim v0.12.4): six digits of precision with
/// trailing zeros stripped (but at least one fractional digit kept), switching to Vim-style scientific
/// notation (`1.0e-4`, `1.234568e7` — no `+`, no leading zeros in the exponent) when the decimal exponent
/// is `<= -4` or `>= 7`.
fn format_vim_float(v: f64) -> String {
    if v == 0.0 {
        // nvim keeps the sign of a negative zero: `-0.0` -> `-0.0`.
        return if v.is_sign_negative() { "-0.0" } else { "0.0" }.to_string();
    }
    // Non-finite values print as nvim's do: `1.0e400` and an overflowing multiply give `inf` / `-inf`.
    if !v.is_finite() {
        return if v.is_nan() {
            "nan".to_string()
        } else if v < 0.0 {
            "-inf".to_string()
        } else {
            "inf".to_string()
        };
    }
    let neg = v < 0.0;
    let a = v.abs();
    // The decimal exponent from Rust's shortest scientific form ("1.2345e3" -> 3) — reliable at the
    // fixed/scientific threshold in a way `log10().floor()` is not (rounding at exact powers of ten).
    let exp: i32 = format!("{a:e}")
        .split_once('e')
        .and_then(|(_, e)| e.parse().ok())
        .unwrap_or(0);
    let body = if exp <= -4 || exp >= 7 {
        // Scientific: six fractional digits in the mantissa, then strip. Rust's `{:e}` already prints the
        // exponent Vim-style (no `+`, no leading zeros), so we only tidy the mantissa.
        let s = format!("{a:.6e}");
        match s.split_once('e') {
            Some((mant, e)) => format!("{}e{e}", strip_fractional_zeros(mant)),
            None => strip_fractional_zeros(&s),
        }
    } else {
        strip_fractional_zeros(&format!("{a:.6}"))
    };
    if neg {
        format!("-{body}")
    } else {
        body
    }
}

/// Strip trailing zeros from a fixed-point fraction, keeping at least one digit after the point
/// (`"1.500000"` -> `"1.5"`, `"1000000.000000"` -> `"1000000.0"`, `"3.333333"` -> `"3.333333"`). The
/// leading integer part's own zeros are protected by the decimal point, so `"100.000000"` -> `"100.0"`.
fn strip_fractional_zeros(s: &str) -> String {
    if !s.contains('.') {
        return format!("{s}.0");
    }
    let trimmed = s.trim_end_matches('0');
    if trimmed.ends_with('.') {
        format!("{trimmed}0")
    } else {
        trimmed.to_string()
    }
}

// ---------------------------------------------------------------------------------------------------------
// Lexer
// ---------------------------------------------------------------------------------------------------------

#[derive(Clone, PartialEq, Debug)]
enum Token {
    Int(i64),
    Float(f64),
    Str(Vec<u8>),
    /// A variable or function name (`[A-Za-z_#][A-Za-z0-9_#:]*`); ruse defines none.
    Name(String),
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Dot,
    LParen,
    RParen,
    /// A character no token starts with (`!`, `$`, `@`, `[`, …). Always the last token.
    Unknown,
    /// Text the lexer could not read (a malformed number, an unterminated string), carrying the error nvim
    /// gives when it reads that text as an operand. Always the last token.
    Bad(Box<ExprError>),
}

/// Tokenize `input` into tokens and the byte offset each starts at. Whitespace separates tokens and is
/// otherwise ignored. A `.` immediately between two digit runs (`3.0`) is a float; a `.` anywhere else is
/// the concatenation operator (`2.'x'`, `'a'.'b'`). A number right after the `.` operator (signs and blanks
/// between allowed) is never a float, as nvim's `eval7` reads that operand with `want_string`: `1.5.2` is
/// `1 . 5 . 2` -> `152`. Lexing stops at the first text it cannot read, ending the list with a
/// [`Token::Unknown`] or [`Token::Bad`] there; whether that is an error is the parser's call.
fn lex(input: &str) -> Vec<(Token, usize)> {
    let bytes = input.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    let mut after_dot = false;
    while i < bytes.len() {
        let c = bytes[i];
        let want_string = after_dot;
        after_dot = match c {
            b'.' => true,
            b' ' | b'\t' | b'\r' | b'\n' | b'+' | b'-' => after_dot,
            _ => false,
        };
        let single = match c {
            b' ' | b'\t' | b'\r' | b'\n' => {
                i += 1;
                continue;
            }
            b'+' => Some(Token::Plus),
            b'-' => Some(Token::Minus),
            b'*' => Some(Token::Star),
            b'/' => Some(Token::Slash),
            b'%' => Some(Token::Percent),
            b'(' => Some(Token::LParen),
            b')' => Some(Token::RParen),
            _ => None,
        };
        if let Some(tok) = single {
            tokens.push((tok, i));
            i += 1;
            continue;
        }
        let lexed = match c {
            // `..` is the same concatenation as `.` (`:help expr-..`, #1342).
            b'.' => Ok((
                Token::Dot,
                i + if bytes.get(i + 1) == Some(&b'.') {
                    2
                } else {
                    1
                },
            )),
            b'\'' => lex_single_quote(bytes, i),
            b'"' => lex_double_quote(bytes, i),
            b'0'..=b'9' => lex_number(bytes, i, want_string),
            c if is_name_start(c) => {
                let end = (i + 1..bytes.len())
                    .find(|&j| !is_name_char(bytes[j]))
                    .unwrap_or(bytes.len());
                Ok((Token::Name(input[i..end].to_string()), end))
            }
            _ => {
                tokens.push((Token::Unknown, i));
                break;
            }
        };
        match lexed {
            Ok((tok, next)) => {
                tokens.push((tok, i));
                i = next;
            }
            Err(e) => {
                tokens.push((Token::Bad(Box::new(e)), i));
                break;
            }
        }
    }
    tokens
}

/// Whether `c` starts a name (nvim's `eval_isnamec1`, plus the `#` of an autoload name, which nvim also
/// reads as a name: `#` is `E121: Undefined variable: #`).
fn is_name_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c == b'#'
}

/// Whether `c` continues a name (nvim's `eval_isnamec`: letters, digits, `_`, `#` and a scope's `:`).
fn is_name_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'#' | b':')
}

/// The input from `start` to the end, the text nvim quotes for a literal it cannot read there.
fn rest(bytes: &[u8], start: usize) -> String {
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

/// Lex a number starting at `start` (on an ASCII digit) as nvim's `eval_number` does. A float is digits,
/// `.`, digits and an optional `[eE][+-]?digits` exponent that does not run on into a letter or another `.`
/// (so `1.5x` and `1.5.2` are not floats, and `1e3` needs a fraction); `want_string`, the operand of the
/// `.` operator, rules a float out. Anything else is an integer read by [`str2nr`], and a letter or digit
/// right after it is an error, as nvim's strict read makes it (`0b12`, `0o8`, `0x`, `1e3`): `E15` quoting
/// the input from the number on, where nvim's `eval_number` reports it.
fn lex_number(bytes: &[u8], start: usize, want_string: bool) -> Result<(Token, usize), ExprError> {
    if !want_string {
        if let Some(end) = float_end(bytes, start) {
            let text = String::from_utf8_lossy(&bytes[start..end]);
            let f: f64 = text
                .parse()
                .map_err(|_| ExprError::Invalid(rest(bytes, start)))?;
            return Ok((Token::Float(f), end));
        }
    }
    let (n, end) = str2nr(bytes, start);
    if bytes.get(end).is_some_and(u8::is_ascii_alphanumeric) {
        return Err(ExprError::Invalid(rest(bytes, start)));
    }
    Ok((Token::Int(n), end))
}

/// Where the float literal at `start` ends, or `None` when the text there is not one (nvim's
/// `eval_number` test before it calls `strtod`).
fn float_end(b: &[u8], start: usize) -> Option<usize> {
    let digits = |mut i: usize| {
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        i
    };
    let mut p = digits(start);
    if b.get(p) != Some(&b'.') || !b.get(p + 1).is_some_and(u8::is_ascii_digit) {
        return None;
    }
    p = digits(p + 1);
    if matches!(b.get(p), Some(b'e' | b'E')) {
        let mut q = p + 1;
        if matches!(b.get(q), Some(b'+' | b'-')) {
            q += 1;
        }
        if !b.get(q).is_some_and(u8::is_ascii_digit) {
            return None;
        }
        p = digits(q);
    }
    if b.get(p)
        .is_some_and(|&c| c.is_ascii_alphabetic() || c == b'.')
    {
        return None;
    }
    Some(p)
}

/// nvim's `vim_str2nr` with `STR2NR_ALL`, as `eval_number` reads an integer literal at `start` (an ASCII
/// digit): a `0x`/`0X` hex, `0b`/`0B` binary or `0o`/`0O` octal prefix when a digit of that base follows it,
/// a leading `0` octal when every digit of the run is below 8 (`010` is 8, `018` is 18), else decimal.
/// Returns the value and the end of the digits; a magnitude past `i64::MAX` saturates there, as nvim's
/// does (`0xffffffffffffffff` and `99999999999999999999` are both 9223372036854775807).
fn str2nr(b: &[u8], start: usize) -> (i64, usize) {
    let at = |i: usize| b.get(start + i).copied().unwrap_or(0);
    let leading_octal = || {
        let mut run = b[start + 1..]
            .iter()
            .take_while(|c| c.is_ascii_digit())
            .peekable();
        run.peek().is_some() && run.all(|&c| c <= b'7')
    };
    let (radix, mut i): (u32, usize) = match (at(0), at(1)) {
        (b'0', b'x' | b'X') if at(2).is_ascii_hexdigit() => (16, 2),
        (b'0', b'b' | b'B') if matches!(at(2), b'0' | b'1') => (2, 2),
        (b'0', b'o' | b'O') if matches!(at(2), b'0'..=b'7') => (8, 2),
        (b'0', _) if leading_octal() => (8, 1),
        _ => (10, 0),
    };
    let mut un: u64 = 0;
    while let Some(d) = char::from(at(i)).to_digit(radix) {
        un = un
            .checked_mul(u64::from(radix))
            .and_then(|v| v.checked_add(u64::from(d)))
            .unwrap_or(u64::MAX);
        i += 1;
    }
    (i64::try_from(un).unwrap_or(i64::MAX), start + i)
}

/// Lex a single-quoted string starting at the opening quote (`start`). Vim's rule: a doubled `''` is a
/// literal single quote; there are no backslash escapes. An unterminated literal is nvim's E115.
fn lex_single_quote(bytes: &[u8], start: usize) -> Result<(Token, usize), ExprError> {
    let mut i = start + 1;
    let mut out = Vec::new();
    while let Some(&c) = bytes.get(i) {
        if c == b'\'' {
            if bytes.get(i + 1) == Some(&b'\'') {
                out.push(b'\'');
                i += 2;
            } else {
                return Ok((Token::Str(out), i + 1));
            }
        } else {
            out.push(c);
            i += 1;
        }
    }
    Err(ExprError::MissingSingleQuote(rest(bytes, start)))
}

/// Lex a double-quoted string starting at the opening quote (`start`) with nvim's escapes (`:help
/// expr-quote`, verified against nvim v0.12.6):
///
/// - `\b \e \f \n \r \t` (BS, ESC, FF, NL, CR, TAB), `\\` and `\"`;
/// - `\101`: one to three octal digits, one byte (`\777` is 0xff, the value cut to a byte as nvim's is);
/// - `\x41` / `\X41`: one or two hex digits, one raw byte (`\xff` is not UTF-8);
/// - `€` (up to four hex digits) and `\U0001F600` (up to eight): the character's UTF-8 bytes, encoded
///   as nvim's `utf_char2bytes` does (see [`push_utf8`]); with no hex digit, `\x`, `\u` give `x`, `u`;
/// - `\<Esc>`: a key that stands for one character (see [`special_key`]); any other `\<` is a plain `<`;
/// - a backslash before any other character drops: `\z` is `z`.
///
/// A NUL (`\0`, `\x00`, `\u0000`) ends the string, since nvim's strings are C strings. An unterminated
/// literal is nvim's E114.
fn lex_double_quote(bytes: &[u8], start: usize) -> Result<(Token, usize), ExprError> {
    let mut i = start + 1;
    let mut out = Vec::new();
    while let Some(&c) = bytes.get(i) {
        if c == b'"' {
            if let Some(nul) = out.iter().position(|&b| b == 0) {
                out.truncate(nul);
            }
            return Ok((Token::Str(out), i + 1));
        }
        if c != b'\\' {
            out.push(c);
            i += 1;
            continue;
        }
        let Some(&e) = bytes.get(i + 1) else { break };
        i += 2;
        match e {
            b'b' => out.push(0x08),
            b'e' => out.push(0x1b),
            b'f' => out.push(0x0c),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'x' | b'X' | b'u' | b'U' => {
                let max = match e {
                    b'x' | b'X' => 2,
                    b'u' => 4,
                    _ => 8,
                };
                let (mut nr, mut n) = (0u32, 0);
                while n < max {
                    let Some(d) = bytes.get(i).and_then(|&h| char::from(h).to_digit(16)) else {
                        break;
                    };
                    nr = (nr << 4) | d;
                    i += 1;
                    n += 1;
                }
                if n == 0 {
                    out.push(e);
                } else if matches!(e, b'x' | b'X') {
                    out.push(nr.to_le_bytes()[0]);
                } else {
                    // nvim holds the value in a C `int`, so `\UFFFFFFFF` is -1.
                    push_utf8(i32::from_ne_bytes(nr.to_ne_bytes()), &mut out);
                }
            }
            b'0'..=b'7' => {
                let mut nr = u32::from(e - b'0');
                for _ in 0..2 {
                    match bytes.get(i) {
                        Some(&d @ b'0'..=b'7') => {
                            nr = nr * 8 + u32::from(d - b'0');
                            i += 1;
                        }
                        _ => break,
                    }
                }
                out.push(nr.to_le_bytes()[0]);
            }
            b'<' => match special_key(&bytes[i..]) {
                Some((key, len)) => {
                    out.extend_from_slice(&key);
                    i += len;
                }
                None => out.push(b'<'),
            },
            // Any other escaped byte stands for itself; a multi-byte character's continuation bytes follow
            // as plain bytes.
            other => out.push(other),
        }
    }
    Err(ExprError::MissingDoubleQuote(rest(bytes, start)))
}

/// nvim's `utf_char2bytes`: a value below 0x80 (a negative one too, which nvim's signed compare lets
/// through) is one byte, cut from the value; above, the 2- to 6-byte form of the original UTF-8, which
/// also encodes surrogates and values past U+10FFFF (`\uD800` is `ed a0 80`, `\U7FFFFFFF` six bytes).
fn push_utf8(c: i32, out: &mut Vec<u8>) {
    let Ok(u) = u32::try_from(c) else {
        out.push(c.to_le_bytes()[0]);
        return;
    };
    let (n, lead): (u32, u32) = match u {
        0..=0x7f => (1, 0),
        0x80..=0x7ff => (2, 0xc0),
        0x800..=0xffff => (3, 0xe0),
        0x1_0000..=0x1f_ffff => (4, 0xf0),
        0x20_0000..=0x3ff_ffff => (5, 0xf8),
        _ => (6, 0xfc),
    };
    let byte = |v: u32| v.to_le_bytes()[0];
    out.push(byte(lead | (u >> (6 * (n - 1)))));
    for k in (0..n - 1).rev() {
        out.push(byte(0x80 | ((u >> (6 * k)) & 0x3f)));
    }
}

/// The key a `\<...>` escape names, from `rest` (the text after `\<`): its bytes and how much of `rest` it
/// spans, through the `>`. Only the keys that stand for one character are read, with nvim's case-blind
/// names: `<Esc>`, `<CR>`/`<Return>`/`<Enter>`, `<NL>`/`<LF>`/`<Linefeed>`, `<Tab>`, `<Space>`, `<lt>`,
/// `<Bslash>`, `<Bar>`, `<Char-N>` (decimal, `0`-octal or `0x`-hex), `<C-x>` for a letter or one of
/// `[ \ ] ^ _ ?`, and `<S-x>` for a letter.
///
/// `None` leaves a plain `<`, as nvim does for a name it does not know (`"\<Bogus>"` is `<Bogus>`). Divergence:
/// the keys nvim encodes as a K_SPECIAL byte sequence (`<BS>`, `<Del>`, `<Left>`, `<F1>`, `<C-@>`, `<M-a>`,
/// `<C-Space>`, `<S-Tab>`, ...) take that path here too, and stay text.
fn special_key(rest: &[u8]) -> Option<(Vec<u8>, usize)> {
    // A key name is short; a bound keeps a long run of `\<` without `>` linear.
    let gt = rest.iter().take(32).position(|&c| c == b'>')?;
    let mut body = &rest[..gt];
    let (mut ctrl, mut shift) = (false, false);
    while body.len() > 2 && body[1] == b'-' {
        match body[0].to_ascii_lowercase() {
            b'c' => ctrl = true,
            b's' => shift = true,
            _ => return None,
        }
        body = &body[2..];
    }
    let key: Vec<u8> = match (ctrl, shift, body) {
        (true, false, &[c]) => match c {
            b'a'..=b'z' | b'A'..=b'Z' | b'[' | b'\\' | b']' | b'^' | b'_' => vec![c & 0x1f],
            b'?' => vec![0x7f],
            _ => return None,
        },
        (false, true, &[c]) if c.is_ascii_alphabetic() => vec![c.to_ascii_uppercase()],
        (false, false, name) => named_key(name)?,
        _ => return None,
    };
    Some((key, gt + 1))
}

/// The bytes of an unmodified key name in [`special_key`]'s set, case-blind.
fn named_key(name: &[u8]) -> Option<Vec<u8>> {
    let lower = name.to_ascii_lowercase();
    let c = match lower.as_slice() {
        b"esc" => 0x1b,
        b"cr" | b"return" | b"enter" => b'\r',
        b"nl" | b"lf" | b"linefeed" => b'\n',
        b"tab" => b'\t',
        b"space" => b' ',
        b"lt" => b'<',
        b"bslash" => b'\\',
        b"bar" => b'|',
        _ => {
            let digits = lower.strip_prefix(b"char-")?;
            if !digits.first().is_some_and(u8::is_ascii_digit) {
                return None;
            }
            let (n, end) = str2nr(digits, 0);
            if end != digits.len() {
                return None;
            }
            let mut out = Vec::new();
            push_utf8(i32::try_from(n).ok()?, &mut out);
            return Some(out);
        }
    };
    Some(vec![c])
}

// ---------------------------------------------------------------------------------------------------------
// Parser (recursive descent). Precedence, loosest to tightest:
//   concat `.`  <  additive `+ -`  <  multiplicative `* / %`  <  unary `- +`  <  primary
// All binary levels are left-associative.
// ---------------------------------------------------------------------------------------------------------

struct Parser<'a> {
    /// The whole expression, which an `E15` quotes.
    input: &'a str,
    /// The tokens and the byte offset each starts at.
    tokens: &'a [(Token, usize)],
    pos: usize,
    /// Open parentheses around the current position.
    depth: usize,
}

/// Parenthesis nesting at which nvim gives E1169 "Expression too recursive" (999 levels evaluate).
const MAX_DEPTH: usize = 1000;

impl Parser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos).map(|(t, _)| t)
    }

    fn bump(&mut self) -> Option<&Token> {
        let t = self.tokens.get(self.pos).map(|(t, _)| t);
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    /// concat := additive ( '.' additive )*  — coerces both sides to strings.
    fn parse_concat(&mut self) -> Result<Value, ExprError> {
        let mut left = self.parse_additive()?;
        while matches!(self.peek(), Some(Token::Dot)) {
            self.bump();
            let right = self.parse_additive()?;
            let mut s = left.coerce_str();
            s.extend_from_slice(&right.coerce_str());
            left = Value::Str(s);
        }
        Ok(left)
    }

    /// additive := term ( ('+' | '-') term )*
    fn parse_additive(&mut self) -> Result<Value, ExprError> {
        let mut left = self.parse_term()?;
        loop {
            let op = match self.peek() {
                Some(Token::Plus) => '+',
                Some(Token::Minus) => '-',
                _ => break,
            };
            self.bump();
            let right = self.parse_term()?;
            left = arith(left, right, op)?;
        }
        Ok(left)
    }

    /// term := unary ( ('*' | '/' | '%') unary )*
    fn parse_term(&mut self) -> Result<Value, ExprError> {
        let mut left = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Some(Token::Star) => '*',
                Some(Token::Slash) => '/',
                Some(Token::Percent) => '%',
                _ => break,
            };
            self.bump();
            let right = self.parse_unary()?;
            left = arith(left, right, op)?;
        }
        Ok(left)
    }

    /// unary := ('-' | '+')* primary — a loop, not recursion, so a long run of signs cannot overflow the
    /// stack (nvim takes any number of them, #756). The signs apply innermost first.
    fn parse_unary(&mut self) -> Result<Value, ExprError> {
        let mut signs = Vec::new();
        while let Some(t @ (Token::Minus | Token::Plus)) = self.peek() {
            signs.push(*t == Token::Minus);
            self.bump();
        }
        let mut v = self.parse_primary()?;
        for minus in signs.into_iter().rev() {
            v = if minus {
                negate(v)?
            } else {
                // Unary plus only validates that the operand is numeric (matching arithmetic's type rule).
                match v {
                    Value::Int(_) | Value::Float(_) => v,
                    Value::Str(_) => {
                        return Err(ExprError::Type("unary '+' needs a number".into()))
                    }
                }
            };
        }
        Ok(v)
    }

    /// primary := Int | Float | Str | '(' concat ')'. A name is an undefined variable, or an unknown
    /// function before a `(`; text the lexer could not read reports its own error; anything else here (an
    /// operator, `)`, an unknown character, the end) is `E15` quoting the whole expression.
    fn parse_primary(&mut self) -> Result<Value, ExprError> {
        let tokens = self.tokens;
        let Some((tok, at)) = tokens.get(self.pos) else {
            return Err(ExprError::Invalid(self.input.to_string()));
        };
        self.pos += 1;
        match tok {
            Token::Int(n) => Ok(Value::Int(*n)),
            Token::Float(f) => Ok(Value::Float(*f)),
            Token::Str(s) => Ok(Value::Str(s.clone())),
            Token::Name(name) if self.peek() == Some(&Token::LParen) => {
                Err(ExprError::UnknownFunction(name.clone()))
            }
            Token::Name(name) => Err(ExprError::UndefinedVariable(name.clone())),
            Token::Bad(e) => Err(e.as_ref().clone()),
            Token::LParen => {
                // Nesting is the one recursion left; nvim refuses the 1000th level (E1169, #756), which
                // also keeps a pasted run of `(` from overflowing the stack and aborting the process.
                self.depth += 1;
                if self.depth >= MAX_DEPTH {
                    return Err(ExprError::TooDeep(self.input[at + 1..].to_string()));
                }
                let v = self.parse_concat()?;
                self.depth -= 1;
                match self.bump() {
                    Some(Token::RParen) => Ok(v),
                    _ => Err(ExprError::MissingParen),
                }
            }
            _ => Err(ExprError::Invalid(self.input.to_string())),
        }
    }
}

/// Negate a numeric value; a string operand is a type error.
fn negate(v: Value) -> Result<Value, ExprError> {
    match v {
        Value::Int(i) => i.checked_neg().map(Value::Int).ok_or(ExprError::Overflow),
        Value::Float(f) => Ok(Value::Float(-f)),
        Value::Str(_) => Err(ExprError::Type("unary '-' needs a number".into())),
    }
}

/// Apply an arithmetic operator to two values. Both must be numeric (no string->number coercion). Int op
/// Int stays Int (integer division truncates toward zero; `%` is the truncated remainder — both matching
/// nvim); any float operand promotes to a float result. `%` is integer-only (Vim's E804 on floats).
fn arith(left: Value, right: Value, op: char) -> Result<Value, ExprError> {
    let (l, r) = match (left, right) {
        (Value::Int(a), Value::Int(b)) => return int_arith(a, b, op),
        (Value::Int(a), Value::Float(b)) => (a as f64, b),
        (Value::Float(a), Value::Int(b)) => (a, b as f64),
        (Value::Float(a), Value::Float(b)) => (a, b),
        _ => {
            return Err(ExprError::Type(format!(
                "operator '{op}' needs numbers (this calculator does not coerce strings)"
            )))
        }
    };
    float_arith(l, r, op)
}

/// Integer `+ - * / %`. Division/modulo by zero errors (Vim's quirks are not copied); `+ - *` wrap on
/// overflow the way Vim's 64-bit ints do, while `/` guards the `i64::MIN / -1` overflow.
fn int_arith(a: i64, b: i64, op: char) -> Result<Value, ExprError> {
    let v = match op {
        '+' => a.wrapping_add(b),
        '-' => a.wrapping_sub(b),
        '*' => a.wrapping_mul(b),
        '/' => {
            if b == 0 {
                return Err(ExprError::DivByZero);
            }
            a.checked_div(b).ok_or(ExprError::Overflow)?
        }
        '%' => {
            if b == 0 {
                return Err(ExprError::DivByZero);
            }
            a.checked_rem(b).ok_or(ExprError::Overflow)?
        }
        _ => unreachable!("arith only passes + - * / %"),
    };
    Ok(Value::Int(v))
}

/// Float `+ - * /`. `%` is rejected on floats (Vim E804). Division by zero errors rather than yielding inf.
fn float_arith(a: f64, b: f64, op: char) -> Result<Value, ExprError> {
    let v = match op {
        '+' => a + b,
        '-' => a - b,
        '*' => a * b,
        '/' => {
            if b == 0.0 {
                return Err(ExprError::DivByZero);
            }
            a / b
        }
        '%' => return Err(ExprError::FloatModulo),
        _ => unreachable!("arith only passes + - * / %"),
    };
    Ok(Value::Float(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assert `input` evaluates to `expected` (the Vim-echo formatting).
    fn ev(input: &str) -> String {
        String::from_utf8(evb(input)).expect("a UTF-8 result")
    }

    /// The raw bytes `input` evaluates to.
    fn evb(input: &str) -> Vec<u8> {
        eval(input).unwrap_or_else(|e| panic!("eval({input:?}) failed: {e}"))
    }

    /// #764: integer literals read as nvim's `eval_number` / `vim_str2nr(STR2NR_ALL)` (nvim v0.12.6).
    #[test]
    fn integer_literals_take_nvims_bases() {
        for (input, want) in [
            ("010", "8"),
            ("010+1", "9"),
            ("-010", "-8"),
            ("0o17", "15"),
            ("0O17", "15"),
            ("08", "8"),
            ("09", "9"),
            ("018", "18"),
            ("00", "0"),
            ("0", "0"),
            ("0x10", "16"),
            ("0xAB", "171"),
            ("0XaB", "171"),
            ("0X1F", "31"),
            ("0b11", "3"),
            ("0B101", "5"),
            ("0x0", "0"),
            ("0b0", "0"),
            ("0o0", "0"),
        ] {
            assert_eq!(ev(input), want, "{input}");
        }
    }

    /// #764: a literal past `i64::MAX` saturates there, in every base, and a minus negates the saturated
    /// value (nvim v0.12.6).
    #[test]
    fn integer_literals_saturate_like_nvim() {
        let max = "9223372036854775807";
        for input in [
            "9223372036854775807",
            "9223372036854775808",
            "99999999999999999999",
            "0xffffffffffffffffff",
            "0x8000000000000000",
            "0b11111111111111111111111111111111111111111111111111111111111111111",
            "0777777777777777777777777",
        ] {
            assert_eq!(ev(input), max, "{input}");
        }
        assert_eq!(ev("-9223372036854775808"), "-9223372036854775807");
        assert_eq!(ev("-99999999999999999999"), "-9223372036854775807");
    }

    /// #764: what nvim refuses (E15/E488) is an error here, so the register is empty: a prefix with no
    /// digit of its base, a digit or letter after the number, and the Vim9-only `'` digit separator.
    #[test]
    fn malformed_number_literals_are_errors() {
        for input in [
            "0x",
            "0b",
            "0o",
            "0b12",
            "0o8",
            "0o18",
            "0xg",
            "00x1",
            "0x-1",
            "1e3",
            "1.e3",
            "1.5e",
            "1.5x",
            "1.5e3x",
            "1.0e+",
            "0.5e1.0",
            "1'000",
            "1'000'000",
            "0x1'0",
            "0b1'1",
            "010'1",
            "1''0",
            "1'",
        ] {
            assert!(eval(input).is_err(), "{input} -> {:?}", eval(input));
        }
    }

    /// #764: float literals and their printing (nvim v0.12.6), negative zero included.
    #[test]
    fn float_literals_match_nvim() {
        for (input, want) in [
            ("1.5e3", "1500.0"),
            ("1.5E3", "1500.0"),
            ("1.5e+3", "1500.0"),
            ("1.5e-3", "0.0015"),
            ("1.0E-2", "0.01"),
            ("-0.0", "-0.0"),
            ("-0.0*1", "-0.0"),
            ("0.0", "0.0"),
            ("010.5", "10.5"),
            ("01.5", "1.5"),
            ("1.0e400", "inf"),
            ("-1.0e400", "-inf"),
            ("1.0e-400", "0.0"),
            ("1.0e308*10", "inf"),
        ] {
            assert_eq!(ev(input), want, "{input}");
        }
    }

    /// #764: a number after the `.` operator is never a float (nvim reads it with `want_string`), and a
    /// non-decimal number ends where its digits do.
    #[test]
    fn concat_operand_numbers_are_not_floats() {
        assert_eq!(ev("1.5.2"), "152");
        assert_eq!(ev("1.5.'x'"), "15x");
        assert_eq!(ev("0x10.5"), "165");
        assert_eq!(ev("0x1.5"), "15");
        assert_eq!(ev("0x1.'x'"), "1x");
        assert_eq!(ev("010.'x'"), "8x");
        assert_eq!(ev("'x'. -1.5"), "x-15");
    }

    /// #764: double-quoted escapes give nvim's bytes (nvim v0.12.6, `"=...<CR>p`).
    #[test]
    fn double_quoted_escapes_match_nvim() {
        for (input, want) in [
            (r#""\x41""#, &b"A"[..]),
            (r#""\X41""#, b"A"),
            (r#""\x4""#, b"\x04"),
            (r#""\x4g""#, b"\x04g"),
            (r#""\x414""#, b"A4"),
            (r#""\x""#, b"x"),
            (r#""\x80""#, b"\x80"),
            (r#""\xff""#, b"\xff"),
            (r#""é""#, "é".as_bytes()),
            (r#""\u41""#, b"A"),
            (r#""€""#, "€".as_bytes()),
            (r#""\u""#, b"u"),
            (r#""\U0001F600""#, "😀".as_bytes()),
            (r#""\U41""#, b"A"),
            (r#""\uD800""#, b"\xed\xa0\x80"),
            (r#""\U110000""#, b"\xf4\x90\x80\x80"),
            (r#""\U200000""#, b"\xf8\x88\x80\x80\x80"),
            (r#""\U3FFFFFF""#, b"\xfb\xbf\xbf\xbf\xbf"),
            (r#""\U4000000""#, b"\xfc\x84\x80\x80\x80\x80"),
            (r#""\U7FFFFFFF""#, b"\xfd\xbf\xbf\xbf\xbf\xbf"),
            (r#""\UFFFFFFFF""#, b"\xff"),
            (r#""\U80000000""#, b""),
            (r#""\e""#, b"\x1b"),
            (r#""\b""#, b"\x08"),
            (r#""\f""#, b"\x0c"),
            (r#""\r""#, b"\r"),
            (r#""\t""#, b"\t"),
            (r#""\n""#, b"\n"),
            (r#""\\""#, b"\\"),
            (r#""\"""#, b"\""),
            (r#""\101""#, b"A"),
            (r#""\1012""#, b"A2"),
            (r#""\1""#, b"\x01"),
            (r#""\777""#, b"\xff"),
            (r#""\8""#, b"8"),
            (r#""\z""#, b"z"),
            (r#""\a""#, b"a"),
            (r#""\v""#, b"v"),
            // A NUL ends the string.
            (r#""\0""#, b""),
            (r#""x\0y""#, b"x"),
            (r#""x\x00y""#, b"x"),
            (r#""\u0000y""#, b""),
            (r#""\400""#, b""),
        ] {
            assert_eq!(evb(input), want, "{input}");
        }
        assert!(eval(r#""a\""#).is_err());
    }

    /// #764: `\<...>` keys that stand for a character give it; an unknown or partial name is plain text,
    /// both as nvim v0.12.6 does.
    #[test]
    fn double_quoted_key_escapes_match_nvim() {
        for (input, want) in [
            (r#""\<Esc>""#, &b"\x1b"[..]),
            (r#""\<esc>""#, b"\x1b"),
            (r#""\<ESC>""#, b"\x1b"),
            (r#""\<CR>""#, b"\r"),
            (r#""\<Return>""#, b"\r"),
            (r#""\<Enter>""#, b"\r"),
            (r#""\<NL>""#, b"\n"),
            (r#""\<LF>""#, b"\n"),
            (r#""\<Linefeed>""#, b"\n"),
            (r#""\<Tab>""#, b"\t"),
            (r#""\<Space>""#, b" "),
            (r#""\<lt>""#, b"<"),
            (r#""\<lt>x""#, b"<x"),
            (r#""\<Bslash>""#, b"\\"),
            (r#""\<Bar>""#, b"|"),
            (r#""\<C-a>""#, b"\x01"),
            (r#""\<c-a>""#, b"\x01"),
            (r#""\<C-A>""#, b"\x01"),
            (r#""\<C-Z>""#, b"\x1a"),
            (r#""\<C-x>""#, b"\x18"),
            (r#""\<C-[>""#, b"\x1b"),
            (r#""\<C-\>""#, b"\x1c"),
            (r#""\<C-]>""#, b"\x1d"),
            (r#""\<C-^>""#, b"\x1e"),
            (r#""\<C-_>""#, b"\x1f"),
            (r#""\<C-?>""#, b"\x7f"),
            (r#""\<S-a>""#, b"A"),
            (r#""\<Char-65>""#, b"A"),
            (r#""\<Char-0101>""#, b"A"),
            (r#""\<Char-233>""#, "é".as_bytes()),
            (r#""\<Bogus>""#, b"<Bogus>"),
            (r#""\<x>""#, b"<x>"),
            (r#""\<>""#, b"<>"),
            (r#""\<C->""#, b"<C->"),
            (r#""\<""#, b"<"),
            (r#""\<Esc""#, b"<Esc"),
            (r#""\<C-A""#, b"<C-A"),
        ] {
            assert_eq!(evb(input), want, "{input}");
        }
    }

    /// Single-quoted strings: `''` is a quote and a backslash is plain text (nvim v0.12.6).
    #[test]
    fn single_quoted_strings_match_nvim() {
        assert_eq!(ev("'it''s'"), "it's");
        assert_eq!(ev("''''"), "'");
        assert_eq!(ev(r"'a\nb'"), r"a\nb");
        assert_eq!(ev("''"), "");
    }

    #[test]
    fn integer_arithmetic_and_precedence() {
        // Verified against nvim v0.12.4 (:echo).
        assert_eq!(ev("1+1"), "2");
        assert_eq!(ev("1+2*3"), "7");
        assert_eq!(ev("(1+2)*3"), "9");
        assert_eq!(ev("2*3+4"), "10");
        assert_eq!(ev("5 - 3 - 1"), "1"); // left-associative
        assert_eq!(ev("8 / 4 / 2"), "1"); // left-associative
        assert_eq!(ev("-3+1"), "-2");
        assert_eq!(ev("2*-3"), "-6");
        assert_eq!(ev("-(2+3)"), "-5");
        assert_eq!(ev("((1))"), "1");
    }

    #[test]
    fn integer_division_truncates_toward_zero_like_vim() {
        assert_eq!(ev("7/2"), "3");
        assert_eq!(ev("3/2"), "1");
        assert_eq!(ev("10/3"), "3");
        assert_eq!(ev("-7/2"), "-3"); // truncation toward zero, not floor
    }

    #[test]
    fn integer_modulo_matches_vim_sign_rules() {
        assert_eq!(ev("10%3"), "1");
        assert_eq!(ev("7%-3"), "1"); // remainder takes the dividend's sign
        assert_eq!(ev("-7%3"), "-1");
    }

    #[test]
    fn float_arithmetic_promotes_and_formats_like_vim() {
        assert_eq!(ev("3.0/2"), "1.5");
        assert_eq!(ev("2+3.0"), "5.0");
        assert_eq!(ev("1.5+1"), "2.5");
        assert_eq!(ev("1.0 * 2"), "2.0");
        assert_eq!(ev("2.5*2"), "5.0");
        assert_eq!(ev("10.0/3"), "3.333333");
        assert_eq!(ev("1.0/3"), "0.333333");
        assert_eq!(ev("2.0/3"), "0.666667");
        assert_eq!(ev("100.0/7"), "14.285714");
    }

    #[test]
    fn float_formatting_edge_cases_match_vim_echo() {
        assert_eq!(ev("1.0"), "1.0");
        assert_eq!(ev("1.5"), "1.5");
        assert_eq!(ev("0.1"), "0.1");
        assert_eq!(ev("0.01"), "0.01");
        assert_eq!(ev("0.001"), "0.001");
        assert_eq!(ev("123456.789"), "123456.789");
        assert_eq!(ev("1000000.0"), "1000000.0");
        assert_eq!(ev("100000.0"), "100000.0");
        assert_eq!(ev("999999.0"), "999999.0");
        assert_eq!(ev("-0.5"), "-0.5");
        assert_eq!(ev("0.0"), "0.0");
        // Scientific switch: |exp| threshold at <= -4 and >= 7 (Vim-style exponent: no '+', no zero pad).
        assert_eq!(ev("0.0001"), "1.0e-4");
        assert_eq!(ev("0.00001"), "1.0e-5");
        assert_eq!(ev("10000000.0"), "1.0e7");
        assert_eq!(ev("100000000.0"), "1.0e8");
        assert_eq!(ev("12345678.0"), "1.234568e7");
        assert_eq!(ev("1.0e10"), "1.0e10");
    }

    #[test]
    fn string_literals_and_concatenation() {
        assert_eq!(ev("'foo'.'bar'"), "foobar");
        assert_eq!(ev("'a' . 'b' . 'c'"), "abc");
        assert_eq!(ev("'x'..'y'"), "xy"); // `..` is `.` (#1342)
        assert_eq!(ev("1..2"), "12");
        assert_eq!(ev("\"foo\".\"bar\""), "foobar");
        assert_eq!(ev("'It''s'"), "It's"); // '' escapes a single quote
        assert_eq!(ev("\"a\\tb\""), "a\tb"); // backslash escape in double quotes
        assert_eq!(ev("\"x\\ny\""), "x\ny");
    }

    #[test]
    fn number_to_string_coercion_in_concat() {
        assert_eq!(ev("'n='.5"), "n=5");
        assert_eq!(ev("'v'.(1+2)"), "v3");
        assert_eq!(ev("1 . 2"), "12");
        assert_eq!(ev("'v'.(3.0)"), "v3.0");
        assert_eq!(ev("'v'.(10.0/4)"), "v2.5");
    }

    #[test]
    fn concat_binds_looser_than_arithmetic_deliberate_divergence() {
        // Documented divergence: `.` is looser than `+`/`-` here (Vim puts it at additive level with
        // string->number coercion). This yields the intuitive calculator result.
        assert_eq!(ev("'n='.1+2"), "n=3");
        assert_eq!(ev("'sum: '.(2+3)*4"), "sum: 20");
    }

    #[test]
    fn errors_degrade_to_empty_via_eval_or_empty() {
        // Parse errors.
        assert!(matches!(eval(""), Err(ExprError::Empty)));
        assert!(matches!(eval("   "), Err(ExprError::Empty)));
        assert!(matches!(eval("1 +"), Err(ExprError::Invalid(_))));
        assert!(matches!(eval("(1+2"), Err(ExprError::MissingParen)));
        assert!(matches!(eval("1 2"), Err(ExprError::Trailing(_))));
        assert!(matches!(eval("2 ** 3"), Err(ExprError::Invalid(_))));
        assert!(matches!(eval("abc"), Err(ExprError::UndefinedVariable(_)))); // no variables
        assert!(matches!(
            eval("'unterminated"),
            Err(ExprError::MissingSingleQuote(_))
        ));
        // Type errors: arithmetic over a string; `%` over a float is nvim's E804.
        assert!(matches!(eval("'a' + 1"), Err(ExprError::Type(_))));
        assert!(matches!(eval("10.0 % 3"), Err(ExprError::FloatModulo)));
        // Division / modulo by zero.
        assert!(matches!(eval("7 / 0"), Err(ExprError::DivByZero)));
        assert!(matches!(eval("5 % 0"), Err(ExprError::DivByZero)));
        // eval_or_empty flattens every error to "".
        assert_eq!(eval_or_empty("1 +"), b"");
        assert_eq!(eval_or_empty("7 / 0"), b"");
        assert_eq!(eval_or_empty("'a' + 1"), b"");
        // ...but still returns the value on success.
        assert_eq!(eval_or_empty("1+2*3"), b"7");
        assert_eq!(eval_or_empty("'n='.5"), b"n=5");
    }

    #[test]
    fn concat_operator_is_distinct_from_float_point() {
        // `3.0` is a float literal; `2.'x'` keeps the dot as concat (digit-after-dot decides).
        assert_eq!(ev("2.'x'"), "2x");
        assert_eq!(ev("3.0"), "3.0");
        assert_eq!(ev("'x'.2"), "x2");
    }

    #[test]
    fn unicode_survives_string_literals() {
        assert_eq!(ev("'café'.'x'"), "caféx");
        assert_eq!(ev("'가'.'나'"), "가나");
    }

    #[test]
    fn does_not_panic_on_arbitrary_input() {
        // A crude fuzz: none of these should panic; each is Ok or Err.
        for s in [
            "",
            "()",
            ")(",
            "1.2.3.4",
            "-----1",
            "+++1",
            "%%%",
            "1/0/0",
            "'''''",
            "\"\\",
            "1e",
            "1e+",
            ".5",
            "9999999999999999999999",
            "((((((",
            "))))))",
            "1.0e999",
        ] {
            let _ = eval(s);
        }
    }

    /// #1570: a failure carries nvim's error, number and text, found where nvim finds it. Each row is
    /// nvim v0.12.6's `v:errmsg` after `"={expr}<CR>p` (`nvim -u NONE -i NONE --headless`).
    #[test]
    fn errors_carry_nvims_message() {
        for (input, want) in [
            ("1+", r#"E15: Invalid expression: "1+""#),
            ("0x", r#"E15: Invalid expression: "0x""#),
            ("0b12", r#"E15: Invalid expression: "0b12""#),
            ("0o18", r#"E15: Invalid expression: "0o18""#),
            ("1e3", r#"E15: Invalid expression: "1e3""#),
            ("1.5.", r#"E15: Invalid expression: "1.5.""#),
            ("*1", r#"E15: Invalid expression: "*1""#),
            (")", r#"E15: Invalid expression: ")""#),
            ("$", r#"E15: Invalid expression: "$""#),
            ("(1+", r#"E15: Invalid expression: "(1+""#),
            // A malformed number quotes the input from the number on, as nvim's `eval_number` does.
            ("0x 1", r#"E15: Invalid expression: "0x 1""#),
            ("1+0x", r#"E15: Invalid expression: "0x""#),
            ("1.5e", r#"E15: Invalid expression: "5e""#),
            ("1.5x", r#"E15: Invalid expression: "5x""#),
            ("nosuchvar", "E121: Undefined variable: nosuchvar"),
            ("x+1", "E121: Undefined variable: x"),
            ("1+_", "E121: Undefined variable: _"),
            ("#", "E121: Undefined variable: #"),
            ("abc.def", "E121: Undefined variable: abc"),
            ("foo()", "E117: Unknown function: foo"),
            ("1'000", "E488: Trailing characters: '000"),
            ("1)", "E488: Trailing characters: )"),
            ("1 2", "E488: Trailing characters: 2"),
            ("1 0x", "E488: Trailing characters: 0x"),
            ("1)'abc", "E488: Trailing characters: )'abc"),
            ("1+2 #", "E488: Trailing characters: #"),
            ("(1", "E110: Missing ')'"),
            ("(1 2)", "E110: Missing ')'"),
            (r#""abc"#, r#"E114: Missing quote: "abc"#),
            (r#"1+"abc"#, r#"E114: Missing quote: "abc"#),
            ("'abc", "E115: Missing quote: 'abc"),
            ("1+'abc", "E115: Missing quote: 'abc"),
            ("1.5%2", "E804: Cannot use '%' with Float"),
        ] {
            let err = eval(input).expect_err(input);
            assert_eq!(err.message().as_deref(), Some(want), "{input}");
            assert_eq!(err.code(), want.split(':').next(), "{input}");
        }
    }

    /// An empty expression fails silently (nvim reuses the last one and says nothing); ruse's deliberate
    /// divergences, which nvim evaluates to a value, say why without an E-number.
    #[test]
    fn silent_and_unnumbered_failures() {
        assert_eq!(ExprError::Empty.message(), None);
        assert_eq!(ExprError::Empty.code(), None);
        for input in ["'a'+1", "7/0", "5%0", "-'a'"] {
            let err = eval(input).expect_err(input);
            assert_eq!(err.code(), None, "{input}");
            let msg = err.message().expect("a message");
            assert!(!msg.as_bytes()[1].is_ascii_digit(), "{input}: {msg}");
        }
    }

    /// #756: deep nesting is an error at nvim's limit (999 levels evaluate, 1000 give E1169), and a long run
    /// of parens or signs never overflows the stack. Run on an 8 MiB stack, the main thread's, where the
    /// editor evaluates `"=` (a debug build needs about 2.5 KiB a level, more than a 2 MiB test thread has).
    #[test]
    fn deep_nesting_is_bounded_like_nvim() {
        let run = || {
            let nest = |n: usize| format!("{}1{}", "(".repeat(n), ")".repeat(n));
            assert_eq!(eval(&nest(999)), Ok("1".into()));
            let deep = eval(&nest(1000)).expect_err("1000 levels are too deep");
            assert_eq!(deep, ExprError::TooDeep(format!("1{}", ")".repeat(1000))));
            assert_eq!(deep.code(), Some("E1169"));
            assert!(matches!(
                eval(&"(".repeat(2_000_000)),
                Err(ExprError::TooDeep(_))
            ));
            assert_eq!(
                eval(&format!("{}1", "-".repeat(1_000_001))),
                Ok("-1".into())
            );
            assert_eq!(eval(&format!("{}1", "-+".repeat(500_000))), Ok("1".into()));
            assert!(eval(&format!("{}'a'", "+".repeat(3))).is_err());
        };
        std::thread::Builder::new()
            .stack_size(8 << 20)
            .spawn(run)
            .expect("spawn")
            .join()
            .expect("deep expressions evaluate without a panic");
    }
}
