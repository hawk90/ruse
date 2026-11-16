//! `'cinkeys'`: which typed keys re-indent the line (`in_cinkeys`).

use super::lex::{at, is_wordc, skipwhite, starts};
use super::{CEnv, Ci, Pos};

impl<'a, L: AsRef<[u8]>> Ci<'a, L> {
    /// `in_cinkeys` for the cursor line: whether `'cinkeys'` holds the key `keytyped` for the moment
    /// `when` (`*` before inserting, `!` instead of inserting, ` ` after inserting). `line_is_empty`
    /// accepts the keys with a `0` before them.
    pub(crate) fn in_cinkeys(&mut self, keytyped: CinKey, when: u8, line_is_empty: bool) -> bool {
        let look = self.e.cinkeys.as_bytes();
        let mut i = 0;
        while i < look.len() {
            let mut try_match = match when {
                b'*' => look[i] == b'*',
                b'!' => look[i] == b'!',
                _ => look[i] != b'*',
            };
            if look[i] == b'*' || look[i] == b'!' {
                i += 1;
            }
            let try_match_word;
            if at(look, i) == b'0' {
                try_match_word = try_match;
                if !line_is_empty {
                    try_match = false;
                }
                i += 1;
            } else {
                try_match_word = false;
            }
            let c = at(look, i);
            if c == b'^' && (b'?'..=b'_').contains(&at(look, i + 1)) {
                let ctrl = at(look, i + 1) ^ 0x40;
                if try_match && keytyped == CinKey::Char(ctrl) {
                    return true;
                }
                i += 2;
            } else if c == b'o' {
                if try_match && keytyped == CinKey::OpenForw {
                    return true;
                }
                i += 1;
            } else if c == b'O' {
                if try_match && keytyped == CinKey::OpenBack {
                    return true;
                }
                i += 1;
            } else if c == b'e' {
                if try_match && keytyped == CinKey::Char(b'e') && self.cur.col >= 4 {
                    let p = self.cur_line();
                    let c0 = self.cur.col - 4;
                    if skipwhite(p, 0) == c0 && starts(p, c0, b"else") {
                        return true;
                    }
                }
                i += 1;
            } else if c == b':' {
                if try_match && keytyped == CinKey::Char(b':') {
                    let p = self.cur_line();
                    if self.cin_iscase(p, 0, false)
                        || self.cin_isscopedecl(p, 0)
                        || self.cin_islabel()
                    {
                        return true;
                    }
                    let col = self.cur.col;
                    if col > 2 && at(p, col - 1) == b':' && at(p, col - 2) == b':' {
                        // As if the `:` just typed were a blank (`class::method` was a label before it).
                        let mut q = p.to_vec();
                        q[col - 1] = b' ';
                        let one = [q];
                        let sub = Ci {
                            lines: &one[..],
                            e: self.e,
                            cur: Pos { lnum: 1, col },
                            insert: self.insert,
                            marks: None,
                        };
                        // The label check reads the lines above: splice the line into the buffer.
                        let mut all: Vec<Vec<u8>> =
                            self.lines.iter().map(|l| l.as_ref().to_vec()).collect();
                        all[self.cur.lnum - 1].clone_from(&one[0]);
                        let hit = sub.cin_iscase(&one[0], 0, false)
                            || sub.cin_isscopedecl(&one[0], 0)
                            || {
                                let mut full = Ci {
                                    lines: &all[..],
                                    e: self.e,
                                    cur: self.cur,
                                    insert: self.insert,
                                    marks: None,
                                };
                                full.cin_islabel()
                            };
                        if hit {
                            return true;
                        }
                    }
                }
                i += 1;
            } else if c == b'<' {
                if try_match {
                    let n = at(look, i + 1);
                    if b"<>!*oOe0:".contains(&n) && n != 0 && keytyped == CinKey::Char(n) {
                        return true;
                    }
                    if let Some(k) = special_key(&look[i..]) {
                        if keytyped == k {
                            return true;
                        }
                    }
                }
                while at(look, i) != 0 && at(look, i) != b'>' {
                    i += 1;
                }
                while at(look, i) == b'>' {
                    i += 1;
                }
            } else if c == b'=' && at(look, i + 1) != b',' && at(look, i + 1) != 0 {
                i += 1;
                let icase = at(look, i) == b'~';
                if icase {
                    i += 1;
                }
                let end = look[i..]
                    .iter()
                    .position(|&b| b == b',')
                    .map_or(look.len(), |k| i + k);
                let word = &look[i..end];
                let col = self.cur.col;
                if (try_match || try_match_word) && col >= word.len() && !word.is_empty() {
                    let last = word[word.len() - 1];
                    let eq = |a: u8, b: u8| {
                        if icase {
                            a.eq_ignore_ascii_case(&b)
                        } else {
                            a == b
                        }
                    };
                    let mut matched = false;
                    if let CinKey::Char(k) = keytyped {
                        if eq(k, last) {
                            let line = self.cur_line();
                            let ws = col - word.len();
                            if (ws == 0 || !is_wordc(at(line, ws - 1)))
                                && line.len() >= col
                                && line[ws..col].iter().zip(word).all(|(&a, &b)| eq(a, b))
                            {
                                matched = true;
                            }
                        }
                    }
                    if matched && try_match_word && !try_match {
                        let line = self.cur_line();
                        if skipwhite(line, 0) != col - word.len() {
                            matched = false;
                        }
                    }
                    if matched {
                        return true;
                    }
                }
                i = end;
            } else {
                if try_match && CinKey::Char(c) == keytyped {
                    return true;
                }
                if c != 0 {
                    i += 1;
                }
            }
            // skip_to_option_part: past the `,` and blanks.
            if at(look, i) == b',' {
                i += 1;
            }
            while at(look, i) == b' ' {
                i += 1;
            }
        }
        false
    }
}

/// A key `'cinkeys'` can name: a typed byte, or the `o`/`O` opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CinKey {
    /// A typed byte (a control key as its byte, `^F` is 6).
    Char(u8),
    /// A line opened below (`o`, `<CR>`).
    OpenForw,
    /// A line opened above (`O`).
    OpenBack,
}

/// The key a `<name>` in `'cinkeys'` stands for, for the few names that are one byte.
fn special_key(s: &[u8]) -> Option<CinKey> {
    let end = s.iter().position(|&c| c == b'>')?;
    let name = &s[1..end];
    let k = match name.to_ascii_lowercase().as_slice() {
        b"tab" => b'\t',
        b"cr" | b"enter" | b"return" => b'\r',
        b"space" => b' ',
        b"bar" => b'|',
        b"lt" => b'<',
        b"bslash" => b'\\',
        b"bs" => 8,
        b"esc" => 27,
        [b'c', b'-', c] if c.is_ascii_alphabetic() => c.to_ascii_uppercase() ^ 0x40,
        _ => return None,
    };
    Some(CinKey::Char(k))
}

/// Whether `'cinkeys'` re-indents the caret's line after `key` was typed (or a line was opened), the
/// caret at byte `col` of line `row` (0-based); `line_is_empty`: only blanks before the key.
pub(crate) fn key_reindents<L: AsRef<[u8]>>(
    lines: &[L],
    row: usize,
    col: usize,
    key: CinKey,
    line_is_empty: bool,
    e: &CEnv<'_>,
) -> bool {
    let mut ci = Ci::new(lines, e);
    ci.cur = Pos { lnum: row + 1, col };
    ci.insert = true;
    ci.in_cinkeys(key, b' ', line_is_empty)
}
