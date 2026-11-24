//! Text formatting (#618), ported from nvim v0.12.6's `textformat.c`, `change.c` and `ops.c`:
//!
//! - `'comments'` leaders: [`leader_len`] (`get_leader_len`), [`last_leader_offset`]
//!   (`get_last_leader_offset`) and [`skip_comment`], which `J` uses under `'formatoptions'` `j`.
//! - The comment leader a line opened from a comment takes ([`open_leader`], the leader half of
//!   `open_line`), for `'formatoptions'` `r` / `o` and for a line auto-wrap breaks.
//! - Auto-wrap ([`FmtBuf::internal_format`]) and `gq` / `gw` ([`FmtBuf::format_lines`]) over a small line
//!   buffer, with `'textwidth'` / `'wrapmargin'`, the comment leaders, `'joinspaces'`, and the
//!   `'formatoptions'` flags `t c q 2 n l 1 p` (`formatlistpat` for `n`). The phases live in children:
//!   `internal_format` (auto-wrap) and `format_lines` (`gq` / `gw`); this file keeps the line buffer's
//!   edit primitives they share (`set_indent`, `do_join`, `open_line`, `get_number_indent`).
//! - `gc` ([`toggle_comment`], nvim's `vim._comment`) in the `toggle_comment` child.
//!
//! Not modelled: the `'formatoptions'` flags `a` (auto-format a paragraph), `w` (trailing blank continues
//! a paragraph), `v` / `b` (Vi-compatible wrap of blanks typed this insert), `m` / `B`'s break at a
//! multibyte character, and `]`; nroff macros in a paragraph start (`'paragraphs'`, as ruse's `{`/`}`);
//! `'smartindent'`'s extra indent on a wrapped line.

use super::indent::indent_cols;
use super::range::cluster_cells;
use super::IndentConfig;
use crate::cells::Tabs;

mod format_lines;
mod internal_format;
mod open_leader;
mod toggle_comment;
pub(crate) use open_leader::{open_leader, OpenLeader};
pub(crate) use toggle_comment::toggle_comment;

/// nvim's default `'comments'`.
pub(crate) const DEFAULT_COMMENTS: &str = "s1:/*,mb:*,ex:*/,://,b:#,:%,:XCOMM,n:>,fb:-,fb:•";

/// nvim's default `'formatlistpat'`.
pub(crate) const DEFAULT_LISTPAT: &str = r"^\s*\d\+[\]:.)}\t ]\s*";

/// One `{flags}:{string}` part of `'comments'`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ComPart {
    pub(crate) flags: String,
    pub(crate) text: Vec<u8>,
}

impl ComPart {
    fn has(&self, f: char) -> bool {
        self.flags.contains(f)
    }
}

/// `'comments'` split into its parts (nvim's `copy_option_part` over `,`, with `\,` an escaped comma and
/// the blanks after a comma skipped). A part without a `:` is dropped, as `get_leader_len` skips it.
pub(crate) fn parse_comments(s: &str) -> Vec<ComPart> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let mut part = Vec::new();
        while i < b.len() && b[i] != b',' {
            if b[i] == b'\\' && b.get(i + 1) == Some(&b',') {
                i += 1;
            }
            part.push(b[i]);
            i += 1;
        }
        if i < b.len() {
            i += 1;
        }
        while i < b.len() && b[i] == b' ' {
            i += 1;
        }
        if let Some(colon) = part.iter().position(|&c| c == b':') {
            out.push(ComPart {
                flags: String::from_utf8_lossy(&part[..colon]).into_owned(),
                text: part[colon + 1..].to_vec(),
            });
        }
    }
    out
}

/// Why a `'formatoptions'` / `'comments'` / `'commentstring'` value is rejected; `Display` is nvim's
/// message.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OptionValueError {
    /// `E539: Illegal character <c>`: a letter no flag uses.
    IllegalChar(char),
    /// `E524: Missing colon`: a `'comments'` part without `:`.
    MissingColon,
    /// `E525: Zero length string`: a `'comments'` part with an empty leader.
    ZeroLength,
    /// `E537`: a `'commentstring'` without `%s`.
    NoPercentS,
}

impl std::fmt::Display for OptionValueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OptionValueError::IllegalChar(c) => write!(f, "E539: Illegal character <{c}>"),
            OptionValueError::MissingColon => f.write_str("E524: Missing colon"),
            OptionValueError::ZeroLength => f.write_str("E525: Zero length string"),
            OptionValueError::NoPercentS => {
                f.write_str("E537: 'commentstring' must be empty or contain %s")
            }
        }
    }
}

/// Whether `'comments'` is valid (nvim's `did_set_comments`).
pub(crate) fn check_comments(s: &str) -> Result<(), OptionValueError> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && b[i] != b':' {
            let c = b[i];
            if !(b"nbsmexflrO".contains(&c) || c.is_ascii_digit() || c == b'-') {
                return Err(OptionValueError::IllegalChar(c as char));
            }
            i += 1;
        }
        if i >= b.len() {
            return Err(OptionValueError::MissingColon);
        }
        i += 1;
        if i >= b.len() || b[i] == b',' {
            return Err(OptionValueError::ZeroLength);
        }
        while i < b.len() && b[i] != b',' {
            if b[i] == b'\\' && i + 1 < b.len() {
                i += 1;
            }
            i += 1;
        }
        if i < b.len() {
            i += 1;
        }
        while i < b.len() && b[i] == b' ' {
            i += 1;
        }
    }
    Ok(())
}

fn white(c: u8) -> bool {
    c == b' ' || c == b'\t'
}

fn skipwhite(line: &[u8], mut i: usize) -> usize {
    while i < line.len() && white(line[i]) {
        i += 1;
    }
    i
}

/// The length of the comment leader `line` starts with (after blanks), and the index of the part that
/// matched (nvim's `get_leader_len`). `backward` is `O` (a part with the `O` flag is skipped);
/// `include_space` counts the blanks after the leader. `(0, None)` when the line is no comment.
pub(crate) fn leader_len(
    line: &[u8],
    parts: &[ComPart],
    backward: bool,
    include_space: bool,
) -> (usize, Option<usize>) {
    let mut result = 0;
    let mut got_com = false;
    let mut flags: Option<usize> = None;
    let mut i = skipwhite(line, 0);
    while i < line.len() {
        let mut found: Option<usize> = None;
        let mut middle_len = 0usize;
        let mut middle_part: Option<usize> = None;
        for (k, part) in parts.iter().enumerate() {
            if middle_len != 0 && !part.has('m') && !part.has('e') {
                break;
            }
            if got_com && !part.has('n') {
                continue;
            }
            if backward && part.has('O') {
                continue;
            }
            let mut s = &part.text[..];
            if s.first().copied().is_some_and(white) {
                if i == 0 || !white(line[i - 1]) {
                    continue;
                }
                s = &s[skipwhite(s, 0)..];
            }
            if !line[i..].starts_with(s) {
                continue;
            }
            let j = s.len();
            if part.has('b') && line.get(i + j).is_some_and(|&c| !white(c)) {
                continue;
            }
            if part.has('m') {
                if middle_len == 0 {
                    middle_len = j;
                    middle_part = Some(k);
                }
                continue;
            }
            if middle_len != 0 && j > middle_len {
                middle_len = 0;
            }
            if middle_len == 0 {
                i += j;
            }
            found = Some(k);
            break;
        }
        if middle_len != 0 {
            i += middle_len;
            found = middle_part;
        }
        let Some(k) = found else {
            break;
        };
        if !got_com {
            flags = Some(k);
        }
        result = i;
        i = skipwhite(line, i);
        if include_space {
            result = i;
        }
        got_com = true;
        if !parts[k].has('n') {
            break;
        }
    }
    (result, flags.filter(|_| result > 0))
}

/// Where the LAST comment leader in `line` starts and the part that matched (nvim's
/// `get_last_leader_offset`), or `None` when the line holds none.
pub(crate) fn last_leader_offset(line: &[u8], parts: &[ComPart]) -> Option<(usize, usize)> {
    let mut result = None;
    let mut lower = 0usize;
    let mut i = line.len();
    while i > lower {
        i -= 1;
        let mut found = None;
        for (k, part) in parts.iter().enumerate() {
            let mut s = &part.text[..];
            if s.first().copied().is_some_and(white) {
                if i == 0 || !white(line[i - 1]) {
                    continue;
                }
                s = &s[skipwhite(s, 0)..];
            }
            if !line[i..].starts_with(s) {
                continue;
            }
            if part.has('b') && line.get(i + s.len()).is_some_and(|&c| !white(c)) {
                continue;
            }
            // A middle part counts only with nothing but blanks before it (a `*` mid-line is no leader).
            if part.has('m') && line[..i].iter().any(|&c| !white(c)) {
                continue;
            }
            found = Some(k);
            break;
        }
        let Some(k) = found else {
            continue;
        };
        result = Some((i, k));
        if parts[k].has('n') {
            continue;
        }
        lower = i;
        // A leader that is the end of another one (`*` inside `/*`): look further left for that one.
        let lead = &parts[k].text[skipwhite(&parts[k].text, 0)..];
        let len1 = lead.len();
        for (m, q) in parts.iter().enumerate() {
            if m == k {
                continue;
            }
            let s = &q.text[skipwhite(&q.text, 0)..];
            let len2 = s.len();
            if len2 == 0 {
                continue;
            }
            let mut off = len2.min(i);
            while off > 0 && off + len1 > len2 {
                off -= 1;
                if s[off..] == lead[..len2 - off] {
                    lower = lower.min(i - off);
                }
            }
        }
    }
    result
}

/// Whether the flags of part `k` (before the `:`) hold `e`, the end of a three-part comment.
fn is_end_part(parts: &[ComPart], k: usize) -> bool {
    parts[k].has('e')
}

/// nvim's `skip_comment`: whether `line` ends inside an unclosed comment, and (when `process`) how many
/// bytes of comment leader at its start a join removes (none for the end of a three-part comment).
pub(crate) fn skip_comment(
    line: &[u8],
    parts: &[ComPart],
    process: bool,
    include_space: bool,
) -> (usize, bool) {
    let is_comment = last_leader_offset(line, parts).is_some_and(|(_, k)| !is_end_part(parts, k));
    if !process {
        return (0, is_comment);
    }
    match leader_len(line, parts, false, include_space) {
        (n, Some(k)) if n > 0 && !is_end_part(parts, k) => (n, is_comment),
        _ => (0, is_comment),
    }
}

/// The 'formatoptions' flags and the other options a format reads.
pub(crate) struct FmtOpts<'a> {
    pub(crate) fo: &'a str,
    pub(crate) parts: Vec<ComPart>,
    pub(crate) ind: IndentConfig,
    pub(crate) joinspaces: bool,
    pub(crate) listpat: Option<crate::pattern::Regex>,
}

impl FmtOpts<'_> {
    fn has(&self, c: char) -> bool {
        self.fo.contains(c)
    }
}

/// A few lines being formatted, the cursor in them, and the position `gw` keeps (both `(line, byte)`).
/// `after` is the buffer line after them, read for context and never changed.
pub(crate) struct FmtBuf {
    pub(crate) lines: Vec<Vec<u8>>,
    pub(crate) cur: (usize, usize),
    pub(crate) saved: Option<(usize, usize)>,
    pub(crate) before: Option<Vec<u8>>,
    pub(crate) after: Option<Vec<u8>>,
    /// Where nvim's last `changed_*` call of the formatting put `'.`, as `(line, byte)` (#1302): a join
    /// leaves it at column 0 of the line after the joined one (its `del_lines`), which may be past `lines`;
    /// a break at the column the line was split at; a changed indent at column 0. `None` until a change.
    pub(crate) last_change: Option<(usize, usize)>,
}

/// One step of a cluster walk over `line`: the byte after the cluster at `i` and its cells at `v`.
fn step(line: &[u8], i: usize, v: usize, ts: Tabs) -> (usize, usize) {
    cluster_cells(line, i, line.len(), v, ts)
}

/// The cells `line[..col]` spans.
fn vcol(line: &[u8], col: usize, ts: Tabs) -> usize {
    let (mut i, mut v) = (0, 0);
    while i < col.min(line.len()) {
        let (j, w) = step(line, i, v, ts);
        i = j;
        v += w;
    }
    v
}

/// The start of the cluster before `col`.
fn prev_col(line: &[u8], col: usize) -> usize {
    let mut i = 0;
    let mut last = 0;
    while i < col {
        last = i;
        i = step(line, i, 0, Tabs::uniform(8)).0;
    }
    last
}

/// The start of the cluster after the one at `col`.
fn next_col(line: &[u8], col: usize) -> usize {
    if col >= line.len() {
        col
    } else {
        step(line, col, 0, Tabs::uniform(8)).0
    }
}

/// The byte of the cluster covering cell `want` (Insert-mode `coladvance`: past the end is the line end).
fn col_at_cell(line: &[u8], want: usize, ts: Tabs) -> usize {
    let (mut i, mut v) = (0, 0);
    while i < line.len() {
        let (j, w) = step(line, i, v, ts);
        if want < v + w {
            return i;
        }
        i = j;
        v += w;
    }
    line.len()
}

/// The char at byte `col` of `line`, a byte that is not UTF-8 read as its Latin-1 char (#1243).
fn char_at(line: &[u8], col: usize) -> Option<char> {
    crate::charclass::cell_char(line, col)
}

/// Move a position on line `l` at or past `from` by `delta` bytes (a mark following an edit).
fn shift_col(p: &mut Option<(usize, usize)>, l: usize, from: usize, delta: isize) {
    if let Some((pl, pc)) = p {
        if *pl == l && *pc >= from {
            *pc = pc.saturating_add_signed(delta).max(from.min(*pc));
        }
    }
}

impl FmtBuf {
    fn line(&self, l: usize) -> Option<&[u8]> {
        if l < self.lines.len() {
            Some(&self.lines[l])
        } else if l == self.lines.len() {
            self.after.as_deref()
        } else {
            None
        }
    }

    fn get_indent(&self, l: usize, o: &FmtOpts<'_>) -> usize {
        self.line(l).map_or(0, |s| indent_cols(s, o.ind.tabs()).0)
    }

    /// nvim's `set_indent(cols, SIN_CHANGED)` on line `l`, moving the cursor and the kept position.
    fn set_indent(&mut self, l: usize, cols: usize, o: &FmtOpts<'_>) {
        let (_, old_len) = indent_cols(&self.lines[l], o.ind.tabs());
        let new = o.ind.rebuilt(&self.lines[l][..old_len], cols);
        if new[..] == self.lines[l][..old_len] {
            return;
        }
        let new_len = new.len();
        self.lines[l].splice(..old_len, new);
        self.last_change = Some((l, 0));
        let fix = |p: &mut (usize, usize)| {
            if p.0 == l {
                if p.1 >= old_len {
                    p.1 = p.1 + new_len - old_len;
                } else if p.1 >= new_len {
                    p.1 = new_len;
                }
            }
        };
        fix(&mut self.cur);
        if let Some(s) = self.saved.as_mut() {
            fix(s);
        }
    }

    /// Delete `n` bytes at the start of line `l` (a comment leader `gq` drops before a join).
    fn del_start(&mut self, l: usize, n: usize) {
        self.lines[l].drain(..n);
        self.last_change = Some((l, 0));
        let fix = |p: &mut (usize, usize)| {
            if p.0 == l {
                p.1 = p.1.saturating_sub(n);
            }
        };
        fix(&mut self.cur);
        if let Some(s) = self.saved.as_mut() {
            fix(s);
        }
    }

    /// nvim's `do_join(2, insert_space, …)` of lines `l` and `l + 1` (no comment-leader removal): the
    /// cursor goes to the seam.
    fn join2(&mut self, l: usize, o: &FmtOpts<'_>) {
        let next = self.lines.remove(l + 1);
        let skip = skipwhite(&next, 0);
        let curr = &next[skip..];
        let first = &self.lines[l];
        let mut spaces = 0usize;
        if !curr.is_empty() && curr[0] != b')' && !first.is_empty() {
            let (end1, end2) = crate::charclass::last_two_chars(first);
            let mut end1 = end1.unwrap_or('\0');
            let end2 = end2.unwrap_or('\0');
            let c0 = char_at(curr, 0).unwrap_or('\0');
            let mbyte_ok = (!o.has('M') || (u32::from(c0) < 0x100 && u32::from(end1) < 0x100))
                && (!o.has('B')
                    || (u32::from(c0) < 0x100 && !eat_space(end1))
                    || (u32::from(end1) < 0x100 && !eat_space(c0)));
            if end1 != '\t' && mbyte_ok {
                if end1 == ' ' {
                    end1 = end2;
                } else {
                    spaces += 1;
                }
                if o.joinspaces && matches!(end1, '.' | '?' | '!') {
                    spaces += 1;
                }
            }
        }
        let base = self.lines[l].len();
        self.lines[l].extend(std::iter::repeat_n(b' ', spaces));
        self.lines[l].extend_from_slice(curr);
        let start = base + spaces;
        let fix = |p: &mut (usize, usize)| {
            if p.0 == l + 1 {
                *p = (l, start + p.1.saturating_sub(skip));
            } else if p.0 > l + 1 {
                p.0 -= 1;
            }
        };
        if let Some(s) = self.saved.as_mut() {
            fix(s);
        }
        self.cur = (l, base);
        self.last_change = Some((l + 1, 0));
    }

    /// Split the cursor line at the cursor for an auto-wrap break (nvim's `open_line(FORWARD,
    /// OPENLINE_DELSPACES | OPENLINE_MARKFIX | OPENLINE_FORMAT [| OPENLINE_DO_COM] [| OPENLINE_COM_LIST])`):
    /// the blanks at the break go, the new line takes the indent and the comment leader. Returns whether
    /// a comment leader was inserted.
    fn open_line(
        &mut self,
        do_com: bool,
        com_list: bool,
        second_line_indent: i32,
        o: &FmtOpts<'_>,
    ) -> bool {
        let (l, col) = self.cur;
        let ts = o.ind.tabs();
        let indent_on = o.ind.autoindent || o.ind.smartindent;
        let saved = self.lines[l][..col].to_vec();
        let extra_full = self.lines[l][col..].to_vec();
        let mut newindent = if indent_on {
            indent_cols(&saved, ts).0
        } else {
            0
        };
        if indent_on && newindent == 0 && !com_list {
            newindent = usize::try_from(second_line_indent).unwrap_or(0);
        }
        let lead = if do_com {
            open_leader(&saved, &o.parts, true, Some(col), indent_on, ts)
        } else {
            OpenLeader::default()
        };
        if let Some(ni) = lead.newindent {
            newindent = ni;
        }
        if lead.leader.is_empty() {
            if let Some(ce) = lead.comment_end {
                // A finished C comment: line up with the line that opened it.
                if saved[ce..].starts_with(b"*/") && indent_on {
                    if let Some(ind) = self.comment_start_indent(l, ce, o) {
                        newindent = ind;
                    }
                }
            }
        }
        let mut leader = lead.leader.clone();
        let did_com = !leader.is_empty();
        if newindent > 0 && did_com {
            let n = skipwhite(&leader, 0);
            leader.drain(..n);
        }
        // DELSPACES: the blanks at the break do not move to the new line.
        let skip = skipwhite(&extra_full, 0);
        let extra = &extra_full[skip..];
        if did_com && com_list && second_line_indent > 0 {
            let used = newindent + leader.len();
            let want = usize::try_from(second_line_indent).unwrap_or(0);
            leader.extend(std::iter::repeat_n(b' ', want.saturating_sub(used)));
        }
        let ws = if newindent > 0 {
            if o.ind.copyindent {
                let old = &saved[..indent_cols(&saved, ts).1];
                o.ind.keeping(old, newindent)
            } else {
                o.ind.whitespace(newindent)
            }
        } else {
            Vec::new()
        };
        let mut new_line = ws.clone();
        new_line.extend_from_slice(&leader);
        let newcol = new_line.len();
        new_line.extend_from_slice(extra);
        self.lines[l].truncate(col);
        self.lines.insert(l + 1, new_line);
        // `open_line`'s `changed_lines(lnum, col, …)` at the break.
        self.last_change = Some((l, col));
        let moved_from = col + skip;
        let fix = |p: &mut (usize, usize)| {
            if p.0 == l && p.1 >= moved_from {
                *p = (l + 1, p.1 - moved_from + newcol);
            } else if p.0 > l {
                p.0 += 1;
            }
        };
        if let Some(s) = self.saved.as_mut() {
            fix(s);
        }
        self.cur = (l + 1, newcol);
        did_com
    }

    /// The indent of the line holding the `/*` that the `*/` at `col` of line `l` closes.
    fn comment_start_indent(&self, l: usize, col: usize, o: &FmtOpts<'_>) -> Option<usize> {
        let mut ll = l;
        let mut end = col;
        loop {
            let line = self.line(ll)?;
            if line[..end.min(line.len())].windows(2).any(|w| w == b"/*") {
                return Some(indent_cols(line, o.ind.tabs()).0);
            }
            if ll == 0 {
                return self
                    .before
                    .as_deref()
                    .filter(|b| b.windows(2).any(|w| w == b"/*"))
                    .map(|b| indent_cols(b, o.ind.tabs()).0);
            }
            ll -= 1;
            end = usize::MAX;
        }
    }

    /// nvim's `get_number_indent`: the cell where a numbered-list item's text starts on line `l`
    /// (`'formatlistpat'` matched after any comment leader), or `None`.
    fn number_indent(&self, l: usize, with_leader: bool, o: &FmtOpts<'_>) -> Option<usize> {
        let line = self.line(l)?;
        let re = o.listpat.as_ref()?;
        let lead = if with_leader {
            leader_len(line, &o.parts, false, true).0
        } else {
            0
        };
        // A byte that is not UTF-8 is one char to the pattern (#1243).
        let hay = crate::pattern::Hay::new(&line[lead..]);
        let m = re.find_at(hay.as_str(), crate::pos::BytePos(0))?;
        let end = lead + hay.to_buf(m.end.0);
        if end >= line.len() {
            return None;
        }
        Some(vcol(line, end, o.ind.tabs()))
    }
}

/// nvim's `utf_eat_space`: punctuation a `B` join puts no space around.
pub(crate) fn eat_space(c: char) -> bool {
    let c = u32::from(c);
    (0x2000..=0x206F).contains(&c)
        || (0x2e00..=0x2e7f).contains(&c)
        || (0x3000..=0x303f).contains(&c)
        || (0xff01..=0xff0f).contains(&c)
        || (0xff1a..=0xff20).contains(&c)
        || (0xff3b..=0xff40).contains(&c)
        || (0xff5b..=0xff65).contains(&c)
}

/// Whether `'formatoptions'` is valid (nvim's `FO_ALL`): the error message if not.
pub(crate) fn check_formatoptions(s: &str) -> Result<(), OptionValueError> {
    match s.chars().find(|c| !"tcro/q2vlb1mMBn,aw]jp".contains(*c)) {
        Some(c) => Err(OptionValueError::IllegalChar(c)),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts() -> Vec<ComPart> {
        parse_comments(DEFAULT_COMMENTS)
    }

    #[test]
    fn default_comments_parse_into_ten_parts() {
        let p = parts();
        assert_eq!(p.len(), 10);
        assert_eq!(p[0].flags, "s1");
        assert_eq!(p[0].text, b"/*");
        assert_eq!(p[9].text, "•".as_bytes());
    }

    #[test]
    fn leader_len_matches_nvim() {
        let p = parts();
        assert_eq!(leader_len(b"// a", &p, false, true).0, 3);
        assert_eq!(leader_len(b"// a", &p, false, false).0, 2);
        assert_eq!(leader_len(b"  # a", &p, false, true).0, 4);
        assert_eq!(leader_len(b"#a", &p, false, true).0, 0, "b: needs a blank");
        assert_eq!(leader_len(b" * b", &p, false, true).0, 3);
        assert_eq!(
            leader_len(b" */", &p, false, true).0,
            3,
            "the end part wins over the middle"
        );
        assert_eq!(leader_len(b"> > a", &p, false, true).0, 4, "n: nests");
        assert_eq!(leader_len(b"x // a", &p, false, true).0, 0);
    }

    #[test]
    fn check_comments_errors() {
        assert!(check_comments(DEFAULT_COMMENTS).is_ok());
        let msg = |s: &str| check_comments(s).unwrap_err().to_string();
        assert_eq!(msg("abc"), "E539: Illegal character <a>");
        assert_eq!(msg(":,"), "E525: Zero length string");
        assert_eq!(msg("b"), "E524: Missing colon");
    }

    #[test]
    fn toggle_comment_round_trips() {
        let out = toggle_comment(&[b"  x", b"    y"], "# %s").unwrap();
        assert_eq!(out, vec![b"  # x".to_vec(), b"  #   y".to_vec()]);
        let back = toggle_comment(&[&out[0], &out[1]], "# %s").unwrap();
        assert_eq!(back, vec![b"  x".to_vec(), b"    y".to_vec()]);
        assert_eq!(
            toggle_comment(&[b"/* x */"], "/*%s*/").unwrap(),
            vec![b" x ".to_vec()]
        );
        assert_eq!(
            toggle_comment(&[b"", b"x"], "# %s").unwrap(),
            vec![b"#".to_vec(), b"# x".to_vec()]
        );
    }
}
