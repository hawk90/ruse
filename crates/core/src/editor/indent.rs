//! Vim's insert-time indent rules (#617), as nvim v0.12.6 runs them: the indent a line opened by
//! `o`/`O`/`<CR>` takes under `'autoindent'`, `'smartindent'` and `'copyindent'` (`open_line`), the
//! realignment `'smartindent'` gives a `{`, `}` or `#` typed first on that line (`ins_try_si`), how
//! `'preserveindent'` and `'shiftround'` change an indent (`set_indent`, `shift_line`), and the
//! `'softtabstop'` / `'smarttab'` steps of an Insert `<Tab>` (`ins_tab`) and `<BS>` (`ins_bs`).
//!
//! Every function here is pure over the buffer bytes; the planner turns the results into edits.
//! A line that continues a `'comments'` leader (`'formatoptions'` `r`/`o`, #618) takes its leader from
//! [`super::format::open_leader`] instead; the planner tries that first.

use super::{IndentConfig, IndentStyle};
use crate::pos::{line_end, line_start};

/// `'cinwords'` at its nvim default: the keywords after which `'smartindent'` indents the next line.
pub(crate) const DEFAULT_CINWORDS: &str = "if,else,while,do,for,switch";

/// The `'smartindent'` state of a line just opened, valid until the first key typed on it (nvim's
/// `can_si`, `can_si_back` and `did_si`, which typing any character clears).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct SiState {
    /// A `}` or `#` typed first realigns the line.
    pub(crate) can_si: bool,
    /// The line was opened ABOVE (`O`) a line not starting with `}`: a `{` typed first may dedent it.
    pub(crate) can_si_back: bool,
    /// The line got the extra `'shiftwidth'` after a `'cinwords'` keyword: a `{` typed first takes it back.
    pub(crate) did_si: bool,
}

impl SiState {
    /// Whether any smartindent rule is armed.
    pub(crate) fn armed(self) -> bool {
        self.can_si || self.can_si_back || self.did_si
    }
}

fn is_white(c: u8) -> bool {
    c == b' ' || c == b'\t'
}

/// A `'iskeyword'` byte at nvim's default (`@,48-57,_,192-255`).
fn is_word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c >= 192
}

/// Leading-whitespace measure of one line (no trailing `\n`): its indent in display COLUMNS (a tab runs
/// to the next `tab_width` multiple) and the BYTE length of that whitespace.
pub(super) fn indent_cols(line: &[u8], tab_width: usize) -> (usize, usize) {
    let tw = tab_width.max(1);
    let (mut cols, mut n) = (0usize, 0usize);
    for &c in line {
        match c {
            b' ' => cols += 1,
            b'\t' => cols += tw - (cols % tw),
            _ => break,
        }
        n += 1;
    }
    (cols, n)
}

/// The line holding byte `at`, without its newline.
fn line_at(b: &[u8], at: usize) -> &[u8] {
    let ls = line_start(b, at);
    &b[ls..line_end(b, ls)]
}

/// The byte offset of the `open` that the `close` expected at `from` would match: scan backwards from
/// `from` counting the pair. Not string- or comment-aware (as nvim's `findmatch` is only loosely).
pub(super) fn find_opener(b: &[u8], from: usize, open: u8, close: u8) -> Option<usize> {
    let mut depth = 1usize;
    let mut i = from.min(b.len());
    while i > 0 {
        i -= 1;
        if b[i] == close {
            depth += 1;
        } else if b[i] == open {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}

/// Whether `line` starts (after blanks) with a keyword of `cinwords` (the comma-separated `'cinwords'`,
/// nvim's `cin_is_cinword`).
fn is_cinword(line: &[u8], cinwords: &str) -> bool {
    let p = &line[line.iter().take_while(|&&c| is_white(c)).count()..];
    cinwords
        .split(',')
        .map(str::as_bytes)
        .filter(|w| !w.is_empty())
        .any(|w| {
            p.starts_with(w)
                && !(p.get(w.len()).copied().is_some_and(is_word) && is_word(w[w.len() - 1]))
        })
}

impl IndentConfig {
    /// The effective `'softtabstop'` in columns: `0` off, a negative value the effective shiftwidth.
    pub(super) fn soft_tab_width(self) -> usize {
        match self.soft_tab {
            0 => 0,
            n if n < 0 => self.shift_width(),
            n => usize::try_from(n).unwrap_or(usize::MAX),
        }
    }

    /// An indent of `cols` columns that keeps the characters of `old` (a line's leading whitespace) as far
    /// as they fit, then fills the rest: a tab to the next stop and further tabs under `noexpandtab`, then
    /// spaces (nvim's `copy_indent`, and `set_indent` under `'preserveindent'`).
    pub(super) fn keeping(self, old: &[u8], cols: usize) -> Vec<u8> {
        let ts = self.tab_width.max(1);
        let (mut out, mut done, mut todo) = (Vec::with_capacity(cols), 0usize, cols);
        for &c in old {
            match c {
                b'\t' => {
                    let pad = ts - done % ts;
                    if todo < pad {
                        break;
                    }
                    todo -= pad;
                    done += pad;
                }
                b' ' if todo > 0 => {
                    todo -= 1;
                    done += 1;
                }
                _ => break,
            }
            out.push(c);
        }
        if self.style == IndentStyle::Tab {
            let pad = ts - done % ts;
            if todo >= pad {
                out.push(b'\t');
                todo -= pad;
            }
            while todo >= ts {
                out.push(b'\t');
                todo -= ts;
            }
        }
        out.extend(std::iter::repeat_n(b' ', todo));
        out
    }

    /// The whitespace a CHANGED indent of `cols` columns gets, over the line's current leading whitespace
    /// `old`: [`IndentConfig::keeping`] under `'preserveindent'`, else rebuilt in the configured style.
    pub(super) fn rebuilt(self, old: &[u8], cols: usize) -> Vec<u8> {
        if self.preserveindent {
            self.keeping(old, cols)
        } else {
            self.whitespace(cols)
        }
    }

    /// The indent `>`/`<` leave a line of `cols` columns at, `times` levels over (nvim's `shift_line`):
    /// under `'shiftround'` a multiple of the shiftwidth (a left shift first drops the extra columns).
    pub(super) fn shifted(self, cols: usize, times: usize, left: bool) -> usize {
        let sw = self.shift_width();
        if !self.shiftround {
            return if left {
                cols.saturating_sub(sw.saturating_mul(times))
            } else {
                cols.saturating_add(sw.saturating_mul(times))
            };
        }
        let (mut i, extra) = (cols / sw, cols % sw);
        let amount = if extra > 0 && left { times - 1 } else { times };
        i = if left {
            i.saturating_sub(amount)
        } else {
            i.saturating_add(amount)
        };
        i.saturating_mul(sw)
    }

    /// The leading whitespace of a line opened from the line at `ls` (nvim's `open_line`), and the
    /// smartindent state it leaves. `end` cuts that line where the cursor splits it (`<CR>`), or is its end
    /// (`o`/`O`); `below` is `o`/`<CR>`. Called only under `'autoindent'` or `'smartindent'`.
    pub(super) fn open_indent(
        self,
        b: &[u8],
        ls: usize,
        end: usize,
        below: bool,
        cinwords: &str,
    ) -> (Vec<u8>, SiState) {
        let ts = self.tab_width;
        let saved = &b[ls..end];
        let mut newindent = indent_cols(saved, ts).0;
        let mut si = SiState::default();
        let mut no_si = false;
        if self.smartindent {
            if below {
                // A preprocessor line takes the indent of the nearest line above that is not one.
                let (mut ptr, mut pls) = (saved, ls);
                if ptr.first() == Some(&b'#') {
                    while ptr.first() == Some(&b'#') && pls > 0 {
                        pls = line_start(b, pls - 1);
                        ptr = line_at(b, pls);
                    }
                    newindent = indent_cols(ptr, ts).0;
                }
                let last = ptr.iter().rposition(|&c| !is_white(c));
                let last_char = last.map(|p| ptr[p]);
                if let Some(mut p) = last {
                    // The character before a final `{` or `;`.
                    if matches!(last_char, Some(b'{' | b';')) {
                        p = p.saturating_sub(1);
                        while p > 0 && is_white(ptr[p]) {
                            p -= 1;
                        }
                    }
                    // A condition split over lines: line up with the line holding its `(`.
                    if ptr[p] == b')' {
                        if let Some(open) = find_opener(b, pls + p, b'(', b')') {
                            pls = line_start(b, open);
                            ptr = line_at(b, pls);
                            newindent = indent_cols(ptr, ts).0;
                        }
                    }
                }
                // After a `{` indent; after a `'cinwords'` keyword too, unless the line ends a statement.
                if last_char == Some(b'{') {
                    si.did_si = true;
                    no_si = true;
                } else if !matches!(last_char, Some(b';' | b'}')) && is_cinword(ptr, cinwords) {
                    si.did_si = true;
                }
            } else {
                // `O` on a preprocessor line takes the indent of the first line below that is not one.
                let mut ptr = saved;
                let mut pls = ls;
                if ptr.first() == Some(&b'#') {
                    let mut continued = false;
                    while ptr.first() == Some(&b'#') || continued {
                        let le = line_end(b, pls);
                        if le >= b.len() || le + 1 >= b.len() {
                            break;
                        }
                        continued = ptr.last() == Some(&b'\\');
                        pls = le + 1;
                        ptr = line_at(b, pls);
                    }
                    newindent = if continued { 0 } else { indent_cols(ptr, ts).0 };
                }
                let first = ptr.iter().find(|&&c| !is_white(c));
                if first == Some(&b'}') {
                    si.did_si = true;
                } else {
                    si.can_si_back = true;
                }
            }
            si.can_si = true;
        }
        if si.did_si {
            let sw = self.shift_width();
            if self.shiftround {
                newindent -= newindent % sw;
            }
            newindent += sw;
        }
        // After a final `{` the extra indent stays when a `{` is typed first (nvim's `no_si`).
        if no_si {
            si.did_si = false;
        }
        let ws = if self.copyindent {
            let old = &saved[..indent_cols(saved, ts).1];
            self.keeping(old, newindent)
        } else {
            self.whitespace(newindent)
        };
        (ws, si)
    }

    /// The leading whitespace the line at `ls` takes when `c` is typed at `cur` with smartindent state
    /// `si` (nvim's `ins_try_si`), or `None` when its indent does not change. `cur` is inside the line's
    /// leading whitespace.
    pub(super) fn typed_indent(
        self,
        b: &[u8],
        ls: usize,
        cur: usize,
        si: SiState,
        c: char,
    ) -> Option<Vec<u8>> {
        let ts = self.tab_width;
        let line = line_at(b, ls);
        let (cols, ws) = indent_cols(line, ts);
        let old = &line[..ws];
        let col = cur - ls;
        if ((si.did_si || si.can_si_back) && c == '{') || (si.can_si && c == '}') {
            if c == '}' {
                if let Some(pos) = find_opener(b, cur, b'{', b'}') {
                    // Line up with the `{` line, or with the line of the `(` a `) {` closes.
                    let pls = line_start(b, pos);
                    let pline = line_at(b, pls);
                    let mut i = pos - pls;
                    while i > 0 {
                        i -= 1;
                        if !is_white(pline[i]) || i == 0 {
                            break;
                        }
                    }
                    let mut target = pline;
                    if pline.get(i) == Some(&b')') {
                        if let Some(open) = find_opener(b, pls + i, b'(', b')') {
                            target = line_at(b, open);
                        }
                    }
                    return Some(self.rebuilt(old, indent_cols(target, ts).0));
                }
            }
            if col > 0 {
                // A `{` after `O` dedents, but not below the indent of the line above it (blank and
                // preprocessor lines skipped).
                let mut dedent = true;
                if c == '{' && si.can_si_back && ls > 0 {
                    let mut pls = ls;
                    while pls > 0 {
                        pls = line_start(b, pls - 1);
                        let p = line_at(b, pls);
                        let first = p.iter().find(|&&x| !is_white(x));
                        if !matches!(first, Some(b'#') | None) {
                            break;
                        }
                    }
                    if indent_cols(line_at(b, pls), ts).0 >= cols {
                        dedent = false;
                    }
                }
                return dedent.then(|| self.rebuilt(old, cols.saturating_sub(self.shift_width())));
            }
            return None;
        }
        // A `#` goes to the first column.
        (si.can_si && c == '#' && col > 0).then(|| self.rebuilt(old, 0))
    }

    /// The `<Tab>` an Insert caret at `cur` types (nvim's `ins_tab`): the bytes that replace `[from, cur)`.
    /// A real tab unless `'expandtab'`, `'softtabstop'` or `'smarttab'` in the indent (at a shiftwidth
    /// unlike the tabstop) asks for columns; then spaces to the next stop, which `noexpandtab` turns back
    /// into tabs over the whitespace run before the caret.
    pub(super) fn tab(self, b: &[u8], ls: usize, cur: usize) -> (usize, Vec<u8>) {
        let ts = self.tab_width.max(1);
        let sw = self.shift_width();
        let sts = self.soft_tab_width();
        let smart = self.smarttab && b[ls..cur].iter().all(|&c| is_white(c));
        let et = self.style == IndentStyle::Space;
        if !et && !(smart && sw != ts) && sts == 0 {
            return (cur, vec![b'\t']);
        }
        let stop = if smart {
            sw
        } else if sts != 0 {
            sts
        } else {
            ts
        };
        let vcol = crate::motion::vcol_of(b, ls, cur, ts);
        let want = vcol + stop - vcol % stop;
        if et {
            return (cur, vec![b' '; want - vcol]);
        }
        let mut from = cur;
        while from > ls && is_white(b[from - 1]) {
            from -= 1;
        }
        let mut v = crate::motion::vcol_of(b, ls, from, ts);
        let mut run = Vec::new();
        while v + (ts - v % ts) <= want {
            run.push(b'\t');
            v += ts - v % ts;
        }
        run.extend(std::iter::repeat_n(b' ', want - v));
        (from, run)
    }

    /// The `<BS>` an Insert caret at `cur` types when `'smarttab'` (in the indent) or `'softtabstop'`
    /// steps it (nvim's `ins_bs`): the span `[from, cur)` to replace and the spaces that replace it, or
    /// `None` for a plain one-character `<BS>`. `typed_space` is whether the character before the caret is
    /// a space typed in this insert (nvim's `inserted_space`), which `'softtabstop'` deletes alone.
    pub(super) fn backspace(
        self,
        b: &[u8],
        ls: usize,
        cur: usize,
        typed_space: bool,
    ) -> Option<(usize, usize)> {
        if cur <= ls {
            return None;
        }
        let ts = self.tab_width.max(1);
        let sts = self.soft_tab_width();
        let prev = b[cur - 1];
        let smart = self.smarttab && b[ls..cur].iter().all(|&c| is_white(c));
        if !(smart || (sts != 0 && (prev == b'\t' || (prev == b' ' && !typed_space)))) {
            return None;
        }
        let vcol = crate::motion::vcol_of(b, ls, cur, ts);
        let stop = if smart { self.shift_width() } else { sts };
        let want = (vcol - 1) / stop * stop;
        let (mut from, mut v) = (cur, vcol);
        while v > want && from > ls && is_white(b[from - 1]) {
            from -= 1;
            v = crate::motion::vcol_of(b, ls, from, ts);
        }
        if from == cur {
            return None;
        }
        Some((from, want.saturating_sub(v)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::ViewOptions;

    fn cfg(f: impl FnOnce(&mut IndentConfig)) -> IndentConfig {
        let mut c = ViewOptions::DEFAULT.indent;
        f(&mut c);
        c
    }

    #[test]
    fn keeping_reuses_old_chars_then_fills() {
        // nvim: "\t  x" (ts=8, et, pi) `>>` sw=4 → "\t      x"; "\t\tx" (noet, pi) `<<` → "\t    x".
        let et = cfg(|c| c.tab_width = 8);
        assert_eq!(et.keeping(b"\t  ", 14), b"\t      ");
        let noet = cfg(|c| {
            c.tab_width = 8;
            c.style = IndentStyle::Tab;
        });
        assert_eq!(noet.keeping(b"\t\t", 12), b"\t    ");
        assert_eq!(noet.keeping(b"  \t", 12), b"  \t    ");
        assert_eq!(noet.keeping(b"  ", 18), b"  \t\t  ");
    }

    #[test]
    fn shiftround_rounds_each_way() {
        let c = cfg(|c| c.shiftround = true);
        assert_eq!(c.shifted(3, 1, false), 4);
        assert_eq!(c.shifted(3, 1, true), 0);
        assert_eq!(c.shifted(6, 2, false), 12);
        assert_eq!(c.shifted(6, 1, true), 4);
        let plain = cfg(|_| {});
        assert_eq!(plain.shifted(3, 1, false), 7);
    }

    #[test]
    fn cinwords_match_whole_keywords() {
        let w = DEFAULT_CINWORDS;
        assert!(is_cinword(b"  if (x)", w));
        assert!(is_cinword(b"switch:", w));
        assert!(!is_cinword(b"  iffy", w));
        assert!(is_cinword(b"for(;;)", w));
        assert!(is_cinword(b"  unless x", "unless"));
        assert!(!is_cinword(b"  if (x)", "unless"));
    }
}
