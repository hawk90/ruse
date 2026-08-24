//! The expression register (`"=`) evaluator — a CLEARLY-SCOPED arithmetic/string calculator, NOT a
//! Vimscript interpreter. Vim's `"=` evaluates a full Vimscript expression (`:help quote=`); ruse has no
//! Vimscript, so faking one would be dishonest. This module covers the genuinely useful common cases and
//! nothing else:
//!
//! - integer and float arithmetic `+ - * / %` with parentheses, unary `-`/`+`, and correct precedence;
//! - Vim's int-vs-float result rules (`1+1`->`2`, `3/2`->`1` truncated integer division, `3.0/2`->`1.5`,
//!   floats printed in Vim's `%g`-like 6-significant form — all verified against nvim v0.12.4);
//! - string literals in single quotes (`''` escapes a quote) or double quotes (`\n \t \r \\ \"` escapes);
//! - the `.` string-concatenation operator with number->string coercion (`'n='.5` -> `n=5`, `'v'.(1+2)`
//!   -> `v3`).
//!
//! Explicitly OUT OF SCOPE (each yields [`ExprError`] -> the register is EMPTY, never a crash): variables,
//! `@r`/`&opt`/`$ENV`/`v:` refs, function/method calls, comparisons and logical/ternary/bitwise operators,
//! lists/dicts/blobs, and the `**` power operator.
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

/// Why an expression did not evaluate to a value. Every variant maps to the same OBSERVABLE outcome — the
/// `"=` register is empty, so the paste/insert does nothing — but the distinct variants let the unit tests
/// pin down exactly which failure mode a malformed expression hits.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ExprError {
    /// The input was empty or only whitespace.
    Empty,
    /// A lexer/parser error: an unexpected character, an unterminated string, a missing operand, an
    /// unbalanced paren, or trailing junk after a complete expression. Carries a short human reason.
    Parse(String),
    /// An arithmetic operator was applied to a string operand (this calculator does not coerce
    /// strings to numbers), or `%` was applied to a float (`:help expr-%` — Vim's E804).
    Type(String),
    /// Integer or float division/modulo by zero (Vim's zero-divisor quirks are intentionally not copied).
    DivByZero,
    /// An integer computation overflowed `i64`.
    Overflow,
}

impl fmt::Display for ExprError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExprError::Empty => write!(f, "empty expression"),
            ExprError::Parse(m) => write!(f, "parse error: {m}"),
            ExprError::Type(m) => write!(f, "type error: {m}"),
            ExprError::DivByZero => write!(f, "division by zero"),
            ExprError::Overflow => write!(f, "integer overflow"),
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
pub fn eval(input: &str) -> Result<String, ExprError> {
    if input.trim().is_empty() {
        return Err(ExprError::Empty);
    }
    let tokens = lex(input)?;
    let mut p = Parser {
        tokens: &tokens,
        pos: 0,
    };
    let value = p.parse_concat()?;
    if p.pos != p.tokens.len() {
        return Err(ExprError::Parse("trailing input after expression".into()));
    }
    Ok(value.into_display())
}

/// Evaluate `input`, yielding the formatted result on success and an EMPTY string on any error — the exact
/// degrade Vim applies to a broken `"=` expression (the paste/insert then inserts nothing). This is the
/// entry point the editor's expression-register commands call.
#[must_use]
pub fn eval_or_empty(input: &str) -> String {
    eval(input).unwrap_or_default()
}

/// A typed intermediate value. The register only ever yields a STRING (its formatted form), but the
/// evaluator carries the int/float/string distinction so arithmetic and `.` apply Vim's type rules.
#[derive(Clone, PartialEq, Debug)]
enum Value {
    Int(i64),
    Float(f64),
    Str(String),
}

impl Value {
    /// The register payload: how Vim `:echo` would render this value.
    fn into_display(self) -> String {
        match self {
            Value::Int(i) => i.to_string(),
            Value::Float(f) => format_vim_float(f),
            Value::Str(s) => s,
        }
    }

    /// The `.`-concatenation coercion: numbers render to their display form, strings pass through.
    fn coerce_str(self) -> String {
        self.into_display()
    }
}

/// Format a float the way Vim `:echo`s it (verified against nvim v0.12.4): six digits of precision with
/// trailing zeros stripped (but at least one fractional digit kept), switching to Vim-style scientific
/// notation (`1.0e-4`, `1.234568e7` — no `+`, no leading zeros in the exponent) when the decimal exponent
/// is `<= -4` or `>= 7`.
fn format_vim_float(v: f64) -> String {
    if v == 0.0 {
        return "0.0".to_string();
    }
    // Non-finite results have no Vim-echo form here (division guards zero divisors; an overflowing multiply
    // is the only other source). Render a stable placeholder rather than "inf"/"NaN" leaking to the buffer.
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
    Str(String),
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Dot,
    LParen,
    RParen,
}

/// Tokenize `input`. Whitespace separates tokens and is otherwise ignored. A `.` immediately between two
/// digit runs (`3.0`) is a float; a `.` anywhere else is the concatenation operator (`2.'x'`, `'a'.'b'`).
fn lex(input: &str) -> Result<Vec<Token>, ExprError> {
    let bytes = input.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            b'+' => {
                tokens.push(Token::Plus);
                i += 1;
            }
            b'-' => {
                tokens.push(Token::Minus);
                i += 1;
            }
            b'*' => {
                tokens.push(Token::Star);
                i += 1;
            }
            b'/' => {
                tokens.push(Token::Slash);
                i += 1;
            }
            b'%' => {
                tokens.push(Token::Percent);
                i += 1;
            }
            b'.' => {
                tokens.push(Token::Dot);
                i += 1;
            }
            b'(' => {
                tokens.push(Token::LParen);
                i += 1;
            }
            b')' => {
                tokens.push(Token::RParen);
                i += 1;
            }
            b'\'' => {
                let (tok, next) = lex_single_quote(input, i)?;
                tokens.push(tok);
                i = next;
            }
            b'"' => {
                let (tok, next) = lex_double_quote(input, i)?;
                tokens.push(tok);
                i = next;
            }
            b'0'..=b'9' => {
                let (tok, next) = lex_number(input, i)?;
                tokens.push(tok);
                i = next;
            }
            other => {
                return Err(ExprError::Parse(format!(
                    "unexpected character {:?}",
                    other as char
                )));
            }
        }
    }
    Ok(tokens)
}

/// Lex a number starting at `start` (on an ASCII digit). Consumes an integer part, an optional `.digits`
/// fraction (only when a digit follows the dot — otherwise the dot is left for the concat operator), and an
/// optional `[eE][+-]?digits` exponent. Yields [`Token::Int`] unless a fraction/exponent makes it a float.
fn lex_number(input: &str, start: usize) -> Result<(Token, usize), ExprError> {
    let bytes = input.as_bytes();
    let mut i = start;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    let mut is_float = false;
    // A fraction only if a digit follows the '.', so `3.0` is a float but `2.'x'` / `2.5.3` keep the dot
    // as the concat operator.
    if i + 1 < bytes.len() && bytes[i] == b'.' && bytes[i + 1].is_ascii_digit() {
        is_float = true;
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
    }
    // An exponent must be followed by (an optional sign and) at least one digit to count.
    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        let mut j = i + 1;
        if j < bytes.len() && (bytes[j] == b'+' || bytes[j] == b'-') {
            j += 1;
        }
        if j < bytes.len() && bytes[j].is_ascii_digit() {
            is_float = true;
            i = j;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
        }
    }
    let text = &input[start..i];
    if is_float {
        let f: f64 = text
            .parse()
            .map_err(|_| ExprError::Parse(format!("invalid float {text:?}")))?;
        Ok((Token::Float(f), i))
    } else {
        let n: i64 = text
            .parse()
            .map_err(|_| ExprError::Parse(format!("integer {text:?} out of range")))?;
        Ok((Token::Int(n), i))
    }
}

/// Lex a single-quoted string starting at the opening quote (`start`). Vim's rule: a doubled `''` is a
/// literal single quote; there are no backslash escapes. An unterminated literal is a parse error.
fn lex_single_quote(input: &str, start: usize) -> Result<(Token, usize), ExprError> {
    let bytes = input.as_bytes();
    let mut i = start + 1;
    let mut out = String::new();
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                out.push('\'');
                i += 2;
            } else {
                return Ok((Token::Str(out), i + 1));
            }
        } else {
            // Push the whole UTF-8 char, not the raw byte, so multi-byte text survives.
            let ch = next_char(input, i);
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    Err(ExprError::Parse("unterminated single-quoted string".into()))
}

/// Lex a double-quoted string starting at the opening quote (`start`). Supports the common backslash
/// escapes `\\ \" \n \t \r`; any other `\x` keeps the character `x` literally (a minimal, useful subset of
/// Vim's escapes). An unterminated literal is a parse error.
fn lex_double_quote(input: &str, start: usize) -> Result<(Token, usize), ExprError> {
    let bytes = input.as_bytes();
    let mut i = start + 1;
    let mut out = String::new();
    while i < bytes.len() {
        match bytes[i] {
            b'"' => return Ok((Token::Str(out), i + 1)),
            b'\\' if i + 1 < bytes.len() => {
                let esc = bytes[i + 1];
                match esc {
                    b'n' => out.push('\n'),
                    b't' => out.push('\t'),
                    b'r' => out.push('\r'),
                    b'\\' => out.push('\\'),
                    b'"' => out.push('"'),
                    _ => {
                        let ch = next_char(input, i + 1);
                        out.push(ch);
                        i += ch.len_utf8();
                        i += 1;
                        continue;
                    }
                }
                i += 2;
            }
            _ => {
                let ch = next_char(input, i);
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    Err(ExprError::Parse("unterminated double-quoted string".into()))
}

/// The Unicode scalar at byte offset `i` (which must be a char boundary). Used so string literals carry
/// multi-byte text through unchanged.
fn next_char(input: &str, i: usize) -> char {
    input[i..].chars().next().unwrap_or('\u{FFFD}')
}

// ---------------------------------------------------------------------------------------------------------
// Parser (recursive descent). Precedence, loosest to tightest:
//   concat `.`  <  additive `+ -`  <  multiplicative `* / %`  <  unary `- +`  <  primary
// All binary levels are left-associative.
// ---------------------------------------------------------------------------------------------------------

struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn bump(&mut self) -> Option<&Token> {
        let t = self.tokens.get(self.pos);
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
            s.push_str(&right.coerce_str());
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

    /// unary := ('-' | '+') unary | primary
    fn parse_unary(&mut self) -> Result<Value, ExprError> {
        match self.peek() {
            Some(Token::Minus) => {
                self.bump();
                let v = self.parse_unary()?;
                negate(v)
            }
            Some(Token::Plus) => {
                self.bump();
                let v = self.parse_unary()?;
                // Unary plus only validates that the operand is numeric (matching arithmetic's type rule).
                match v {
                    Value::Int(_) | Value::Float(_) => Ok(v),
                    Value::Str(_) => Err(ExprError::Type("unary '+' needs a number".into())),
                }
            }
            _ => self.parse_primary(),
        }
    }

    /// primary := Int | Float | Str | '(' concat ')'
    fn parse_primary(&mut self) -> Result<Value, ExprError> {
        match self.bump() {
            Some(Token::Int(n)) => Ok(Value::Int(*n)),
            Some(Token::Float(f)) => Ok(Value::Float(*f)),
            Some(Token::Str(s)) => Ok(Value::Str(s.clone())),
            Some(Token::LParen) => {
                let v = self.parse_concat()?;
                match self.bump() {
                    Some(Token::RParen) => Ok(v),
                    _ => Err(ExprError::Parse("missing ')'".into())),
                }
            }
            Some(t) => Err(ExprError::Parse(format!("unexpected token {t:?}"))),
            None => Err(ExprError::Parse("unexpected end of expression".into())),
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
        '%' => return Err(ExprError::Type("'%' cannot be used with a float".into())),
        _ => unreachable!("arith only passes + - * / %"),
    };
    Ok(Value::Float(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assert `input` evaluates to `expected` (the Vim-echo formatting).
    fn ev(input: &str) -> String {
        eval(input).unwrap_or_else(|e| panic!("eval({input:?}) failed: {e}"))
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
        assert!(matches!(eval("1 +"), Err(ExprError::Parse(_))));
        assert!(matches!(eval("(1+2"), Err(ExprError::Parse(_))));
        assert!(matches!(eval("1 2"), Err(ExprError::Parse(_))));
        assert!(matches!(eval("2 ** 3"), Err(ExprError::Parse(_))));
        assert!(matches!(eval("abc"), Err(ExprError::Parse(_)))); // no variables
        assert!(matches!(eval("'unterminated"), Err(ExprError::Parse(_))));
        // Type errors: arithmetic over a string, `%` over a float.
        assert!(matches!(eval("'a' + 1"), Err(ExprError::Type(_))));
        assert!(matches!(eval("10.0 % 3"), Err(ExprError::Type(_))));
        // Division / modulo by zero.
        assert!(matches!(eval("7 / 0"), Err(ExprError::DivByZero)));
        assert!(matches!(eval("5 % 0"), Err(ExprError::DivByZero)));
        // eval_or_empty flattens every error to "".
        assert_eq!(eval_or_empty("1 +"), "");
        assert_eq!(eval_or_empty("7 / 0"), "");
        assert_eq!(eval_or_empty("'a' + 1"), "");
        // ...but still returns the value on success.
        assert_eq!(eval_or_empty("1+2*3"), "7");
        assert_eq!(eval_or_empty("'n='.5"), "n=5");
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
}
