//! LSP snippet expansion (F-014, completion follow-up). A pure text transform — no editor state — so it is
//! exhaustively unit-tested. Slice 1 expands an LSP snippet body into plain text and reports ONE cursor
//! position (the first tabstop); multi-tabstop `<Tab>` navigation + placeholder selection is slice 2.
//!
//! Grammar (LSP): `$N` / `${N}` (tabstop, empty), `${N:default}` (tabstop with default text), `$0` / `${0}`
//! (final position), `${N|a,b,c|}` (choice — first choice is the default here), `$var` / `${var}` /
//! `${var:default}` (variable — unresolved, so its default text or empty), escapes `\$` `\}` `\\`.

/// The result of expanding a snippet: the plain `text` to insert and the `cursor` BYTE offset within it (the
/// first tabstop, else `$0`, else the end).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expansion {
    pub text: String,
    pub cursor: usize,
}

/// Expand an LSP snippet `body` into plain text + the first-tabstop cursor offset (see the module docs).
pub fn expand(body: &str) -> Expansion {
    let mut p = Parser {
        chars: body.chars().collect(),
        i: 0,
        out: String::new(),
        tabs: Vec::new(),
    };
    p.parse(false);
    let cursor = pick_cursor(&p.tabs, p.out.len());
    Expansion {
        text: p.out,
        cursor,
    }
}

struct Parser {
    chars: Vec<char>,
    i: usize,
    out: String,
    /// `(tabstop number, byte offset in `out` at the tabstop's start)`.
    tabs: Vec<(u32, usize)>,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.i).copied()
    }

    /// Append rendered text until end (or, when `nested`, an unescaped `}` — which the caller consumes).
    fn parse(&mut self, nested: bool) {
        while let Some(c) = self.peek() {
            match c {
                '}' if nested => return,
                '\\' => {
                    self.i += 1;
                    match self.peek() {
                        // Only `$ } \` are escapes; anything else keeps the backslash literally.
                        Some(e @ ('$' | '}' | '\\')) => {
                            self.out.push(e);
                            self.i += 1;
                        }
                        Some(e) => {
                            self.out.push('\\');
                            self.out.push(e);
                            self.i += 1;
                        }
                        None => self.out.push('\\'),
                    }
                }
                '$' => self.parse_dollar(),
                _ => {
                    self.out.push(c);
                    self.i += 1;
                }
            }
        }
    }

    fn parse_dollar(&mut self) {
        self.i += 1; // past '$'
        match self.peek() {
            Some('{') => self.parse_braced(),
            Some(c) if c.is_ascii_digit() => {
                let n = self.read_number();
                self.tabs.push((n, self.out.len())); // zero-width tabstop
            }
            Some(c) if c.is_alphabetic() || c == '_' => {
                // `$variable` — unresolved, renders nothing; consume the identifier.
                while matches!(self.peek(), Some(d) if d.is_alphanumeric() || d == '_') {
                    self.i += 1;
                }
            }
            _ => self.out.push('$'), // a lone '$'
        }
    }

    fn parse_braced(&mut self) {
        self.i += 1; // past '{'
        let n = self.read_number();
        if n != u32::MAX {
            // `${N...}` — a numbered tabstop (read_number returns the sentinel when no digits were present).
            match self.peek() {
                Some(':') => {
                    self.i += 1;
                    self.tabs.push((n, self.out.len())); // cursor at the default's start
                    self.parse(true); // the default may itself contain tabstops/text
                    self.consume('}');
                }
                Some('|') => {
                    self.i += 1;
                    self.tabs.push((n, self.out.len()));
                    let first = self.read_choice_first();
                    self.out.push_str(&first);
                    self.skip_to_choice_end();
                    self.consume('}');
                }
                _ => {
                    // `${N}` (or malformed) — zero-width tabstop.
                    self.tabs.push((n, self.out.len()));
                    self.consume('}');
                }
            }
        } else {
            // `${variable}` / `${variable:default}` — unresolved variable: render its default (or empty).
            while matches!(self.peek(), Some(d) if d != '}' && d != ':') {
                self.i += 1;
            }
            match self.peek() {
                Some(':') => {
                    self.i += 1;
                    self.parse(true);
                    self.consume('}');
                }
                _ => self.consume('}'),
            }
        }
    }

    fn read_number(&mut self) -> u32 {
        let mut n: u32 = 0;
        let mut any = false;
        while let Some(d) = self.peek().and_then(|c| c.to_digit(10)) {
            n = n.saturating_mul(10).saturating_add(d);
            any = true;
            self.i += 1;
        }
        if !any {
            u32::MAX // sentinel: not a number (so the "was digit" check below is false)
        } else {
            n
        }
    }

    /// The first choice of a `${N|a,b,c|}`, honoring `\,`/`\|` escapes; leaves `i` at the first `,` or `|`.
    fn read_choice_first(&mut self) -> String {
        let mut s = String::new();
        while let Some(c) = self.peek() {
            match c {
                ',' | '|' => break,
                '\\' => {
                    self.i += 1;
                    if let Some(e) = self.peek() {
                        s.push(e);
                        self.i += 1;
                    }
                }
                _ => {
                    s.push(c);
                    self.i += 1;
                }
            }
        }
        s
    }

    /// Skip the remaining choices up to and including the closing `|`.
    fn skip_to_choice_end(&mut self) {
        while let Some(c) = self.peek() {
            self.i += 1;
            if c == '|' {
                break;
            }
        }
    }

    fn consume(&mut self, want: char) {
        if self.peek() == Some(want) {
            self.i += 1;
        }
    }
}

/// The cursor: the lowest positive tabstop's offset, else `$0`, else the end of the text.
fn pick_cursor(tabs: &[(u32, usize)], end: usize) -> usize {
    tabs.iter()
        .filter(|(n, _)| *n >= 1 && *n != u32::MAX)
        .min_by_key(|(n, _)| *n)
        .or_else(|| tabs.iter().find(|(n, _)| *n == 0))
        .map_or(end, |(_, off)| *off)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ex(body: &str) -> (String, usize) {
        let e = expand(body);
        (e.text, e.cursor)
    }

    #[test]
    fn final_tabstop_between_parens() {
        assert_eq!(ex("println!($0)"), ("println!()".to_string(), 9));
    }

    #[test]
    fn numbered_tabstop_with_default_renders_default_cursor_at_start() {
        // `${1:name}` → text "name", cursor at its START (byte 0).
        assert_eq!(ex("${1:name}"), ("name".to_string(), 0));
    }

    #[test]
    fn lowest_positive_tabstop_wins_over_zero() {
        // `if $1 {\n\t$0\n}` → cursor at `$1` (byte 3), not `$0`.
        let (text, cur) = ex("if $1 {\n\t$0\n}");
        assert_eq!(text, "if  {\n\t\n}");
        assert_eq!(cur, 3); // right after "if "
    }

    #[test]
    fn choice_uses_first_option() {
        assert_eq!(ex("${1|pub,pub(crate)|} fn"), ("pub fn".to_string(), 0));
    }

    #[test]
    fn escapes_are_literal() {
        assert_eq!(
            ex("cost: \\$5 \\} done"),
            ("cost: $5 } done".to_string(), 15)
        );
    }

    #[test]
    fn no_tabstop_places_cursor_at_end() {
        assert_eq!(ex("plain text"), ("plain text".to_string(), 10));
    }

    #[test]
    fn nested_default_flattens() {
        // `${1:${2:x}}` → text "x"; the lowest positive tabstop is $1 at offset 0.
        assert_eq!(ex("${1:${2:x}}"), ("x".to_string(), 0));
    }

    #[test]
    fn unresolved_variable_renders_default_or_empty() {
        assert_eq!(ex("$TM_FILENAME/x"), ("/x".to_string(), 2));
        assert_eq!(ex("${TM_SELECTED_TEXT:def}"), ("def".to_string(), 3));
    }
}
