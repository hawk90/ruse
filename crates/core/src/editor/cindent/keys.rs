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
            let (try_, key) = item_prefix(look, i, when, line_is_empty);
            match self.match_item(look, key, keytyped, try_) {
                Step::Hit => return true,
                Step::Next(next) => i = skip_to_option_part(look, next),
            }
        }
        false
    }

    /// The key part of one `'cinkeys'` item at `look[i]`, told apart as `in_cinkeys` does: a `^X`
    /// control key, `o`, `O`, `e`, `:`, a `<name>`, a `=word`, or a boring generic character.
    fn match_item(&mut self, look: &[u8], mut i: usize, keytyped: CinKey, try_: TryMatch) -> Step {
        let try_match = try_.try_match;
        let c = at(look, i);
        if c == b'^' && (b'?'..=b'_').contains(&at(look, i + 1)) {
            // Does it look like a control character?
            let ctrl = at(look, i + 1) ^ 0x40;
            if try_match && keytyped == CinKey::Char(ctrl) {
                return Step::Hit;
            }
            i += 2;
        } else if c == b'o' {
            // `o` means the `o` command, open forward.
            if try_match && keytyped == CinKey::OpenForw {
                return Step::Hit;
            }
            i += 1;
        } else if c == b'O' {
            // `O` means the `O` command, open backward.
            if try_match && keytyped == CinKey::OpenBack {
                return Step::Hit;
            }
            i += 1;
        } else if c == b'e' {
            if try_match && keytyped == CinKey::Char(b'e') && self.else_before_cursor() {
                return Step::Hit;
            }
            i += 1;
        } else if c == b':' {
            if try_match && keytyped == CinKey::Char(b':') && self.colon_ends_label() {
                return Step::Hit;
            }
            i += 1;
        } else if c == b'<' {
            return angle_item(look, i, keytyped, try_match);
        } else if c == b'=' && at(look, i + 1) != b',' && at(look, i + 1) != 0 {
            return self.word_item(look, i + 1, keytyped, try_);
        } else {
            // Ok, it's a boring generic character.
            if try_match && CinKey::Char(c) == keytyped {
                return Step::Hit;
            }
            if c != 0 {
                i += 1;
            }
        }
        Step::Next(i)
    }

    /// `e` checks for `else` at the start of the line, just before the cursor.
    fn else_before_cursor(&self) -> bool {
        if self.cur.col < 4 {
            return false;
        }
        let p = self.cur_line();
        let c0 = self.cur.col - 4;
        skipwhite(p, 0) == c0 && starts(p, c0, b"else")
    }

    /// `:` re-indents only at the end of a label or `case`, or when the line was one before the `:`
    /// was typed (to fix `class::method` for C++).
    fn colon_ends_label(&mut self) -> bool {
        let p = self.cur_line();
        if self.cin_iscase(p, 0, false) || self.cin_isscopedecl(p, 0) || self.cin_islabel() {
            return true;
        }
        let col = self.cur.col;
        if !(col > 2 && at(p, col - 1) == b':' && at(p, col - 2) == b':') {
            return false;
        }
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
        let mut all: Vec<Vec<u8>> = self.lines.iter().map(|l| l.as_ref().to_vec()).collect();
        all[self.cur.lnum - 1].clone_from(&one[0]);
        sub.cin_iscase(&one[0], 0, false) || sub.cin_isscopedecl(&one[0], 0) || {
            let mut full = Ci {
                lines: &all[..],
                e: self.e,
                cur: self.cur,
                insert: self.insert,
                marks: None,
            };
            full.cin_islabel()
        }
    }

    /// A `=word` item (`=~word` ignores case), `look[i]` just past the `=`: the key typed ends
    /// `word`, which starts a word; with a `0` before it on a line that is not empty, the word must
    /// also be the first thing on the line.
    fn word_item(&self, look: &[u8], mut i: usize, keytyped: CinKey, try_: TryMatch) -> Step {
        let TryMatch {
            try_match,
            try_match_word,
        } = try_;
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
                return Step::Hit;
            }
        }
        Step::Next(end)
    }
}

/// Whether one `'cinkeys'` item may match: `try_match` for the key itself, `try_match_word` for a
/// `0=word` typed on a line that is not empty.
#[derive(Clone, Copy)]
struct TryMatch {
    try_match: bool,
    try_match_word: bool,
}

/// What matching one `'cinkeys'` item found: the key, or the index just past the item.
enum Step {
    Hit,
    Next(usize),
}

/// The `*`/`!`/`0` prefix of the `'cinkeys'` item at `look[i]`, as `in_cinkeys` reads it: whether
/// to try a match for the moment `when`, and the index of the key after the prefix.
fn item_prefix(look: &[u8], mut i: usize, when: u8, line_is_empty: bool) -> (TryMatch, usize) {
    // Find out if we want to try a match with this key, depending on `when` and a `*` or `!` that
    // may be preceding the key.
    let mut try_match = match when {
        b'*' => look[i] == b'*',
        b'!' => look[i] == b'!',
        _ => look[i] != b'*',
    };
    if look[i] == b'*' || look[i] == b'!' {
        i += 1;
    }
    // A `0` only accepts a match if the line is empty, but may still match when typing the last
    // char of a word.
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
    (
        TryMatch {
            try_match,
            try_match_word,
        },
        i,
    )
}

/// A `<name>` item at `look[i]`: `<>>`-style single bytes and the one-byte key names; the index
/// moves past the `>`.
fn angle_item(look: &[u8], mut i: usize, keytyped: CinKey, try_match: bool) -> Step {
    if try_match {
        let n = at(look, i + 1);
        if b"<>!*oOe0:".contains(&n) && n != 0 && keytyped == CinKey::Char(n) {
            return Step::Hit;
        }
        if let Some(k) = special_key(&look[i..]) {
            if keytyped == k {
                return Step::Hit;
            }
        }
    }
    while at(look, i) != 0 && at(look, i) != b'>' {
        i += 1;
    }
    while at(look, i) == b'>' {
        i += 1;
    }
    Step::Next(i)
}

/// `skip_to_option_part`: past the `,` and the blanks after an item.
fn skip_to_option_part(look: &[u8], mut i: usize) -> usize {
    if at(look, i) == b',' {
        i += 1;
    }
    while at(look, i) == b' ' {
        i += 1;
    }
    i
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
