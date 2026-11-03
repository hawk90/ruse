//! Text formatting (#618), ported from nvim v0.12.6's `textformat.c`, `change.c` and `ops.c`:
//!
//! - `'comments'` leaders: [`leader_len`] (`get_leader_len`), [`last_leader_offset`]
//!   (`get_last_leader_offset`) and [`skip_comment`], which `J` uses under `'formatoptions'` `j`.
//! - The comment leader a line opened from a comment takes ([`open_leader`], the leader half of
//!   `open_line`), for `'formatoptions'` `r` / `o` and for a line auto-wrap breaks.
//! - Auto-wrap ([`FmtBuf::internal_format`]) and `gq` / `gw` ([`FmtBuf::format_lines`]) over a small line
//!   buffer, with `'textwidth'` / `'wrapmargin'`, the comment leaders, `'joinspaces'`, and the
//!   `'formatoptions'` flags `t c q 2 n l 1 p` (`formatlistpat` for `n`).
//!
//! Not modelled: the `'formatoptions'` flags `a` (auto-format a paragraph), `w` (trailing blank continues
//! a paragraph), `v` / `b` (Vi-compatible wrap of blanks typed this insert), `m` / `B`'s break at a
//! multibyte character, and `]`; nroff macros in a paragraph start (`'paragraphs'`, as ruse's `{`/`}`);
//! `'smartindent'`'s extra indent on a wrapped line.

use super::indent::indent_cols;
use super::range::cluster_cells;
use super::IndentConfig;

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

/// What a line opened from `saved` takes from its comment leader (the leader half of nvim's `open_line`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct OpenLeader {
    /// The leader text the new line starts with, blanks included (empty: no leader).
    pub(crate) leader: Vec<u8>,
    /// The indent the new line takes instead of `saved`'s, when the leader was replaced (a start or end
    /// part opens the middle one) under `'autoindent'` / `'smartindent'`.
    pub(crate) newindent: Option<usize>,
    /// The column in `saved` of a comment end (`*/`) that closes the comment: the new line lines up with
    /// the line that opened it.
    pub(crate) comment_end: Option<usize>,
}

/// The comment leader a line opened from `saved` takes (nvim's `open_line` with `OPENLINE_DO_COM`).
/// `saved` is the source line, cut at the cursor when the open splits it (`p_extra`: Insert `<CR>` or an
/// auto-wrap break, with `col` the cursor column); `forward` is `o` / a split, else `O`. `indent_on` is
/// `'autoindent'` or `'smartindent'`.
pub(crate) fn open_leader(
    saved: &[u8],
    parts: &[ComPart],
    forward: bool,
    split: Option<usize>,
    indent_on: bool,
    ts: usize,
) -> OpenLeader {
    let mut out = OpenLeader::default();
    let (mut lead_len, k) = leader_len(saved, parts, !forward, true);
    let Some(k) = k.filter(|_| lead_len > 0) else {
        return out;
    };
    let flags = parts[k].flags.as_bytes();
    let mut lead_repl: Option<Vec<u8>> = None;
    let mut extra_space = false;
    let mut require_blank = false;
    for &f in flags {
        match f {
            b'b' => require_blank = true,
            b's' | b'm' => {
                // The middle and end parts follow a start part, the end part follows a middle one.
                let (middle, end) = if f == b's' {
                    if !forward {
                        lead_len = 0;
                        break;
                    }
                    require_blank = false;
                    (parts.get(k + 1), parts.get(k + 2))
                } else {
                    (parts.get(k), parts.get(k + 1))
                };
                if f == b's' {
                    require_blank = middle.is_some_and(|m| m.has('b'));
                } else {
                    require_blank |= parts[k].has('b');
                }
                let lead_middle = middle.map(|m| m.text.clone()).unwrap_or_default();
                let lead_end = end.map(|e| e.text.clone()).unwrap_or_default();
                // The comment ends on this very line: no leader.
                if forward && !lead_end.is_empty() {
                    if let Some(p) =
                        (lead_len..saved.len()).find(|&p| saved[p..].starts_with(&lead_end))
                    {
                        out.comment_end = Some(p);
                        lead_len = 0;
                    }
                }
                if lead_len > 0 {
                    if f == b's' {
                        lead_repl = Some(lead_middle);
                    }
                    let typed_after = match split {
                        Some(col) => col == lead_len,
                        None => saved.len() == lead_len,
                    };
                    if !white(saved[lead_len - 1]) && (typed_after || require_blank) {
                        extra_space = true;
                    }
                }
                break;
            }
            b'e' => {
                if forward {
                    out.comment_end = Some(skipwhite(saved, 0));
                    lead_len = 0;
                    break;
                }
                // `O` on a comment end opens the middle part, the one before it.
                lead_repl = Some(
                    k.checked_sub(1)
                        .map(|m| parts[m].text.clone())
                        .unwrap_or_default(),
                );
                extra_space = true;
                break;
            }
            b'f' => {
                if forward {
                    lead_repl = Some(Vec::new());
                } else {
                    lead_len = 0;
                }
                break;
            }
            _ => {}
        }
    }
    if lead_len == 0 {
        return out;
    }
    let mut leader = saved[..lead_len].to_vec();
    if let Some(repl) = lead_repl {
        let mut right = false;
        let mut off: i64 = 0;
        let mut p = 0;
        while p < flags.len() {
            match flags[p] {
                b'r' => {
                    right = true;
                    p += 1;
                }
                b'l' => {
                    right = false;
                    p += 1;
                }
                c if c.is_ascii_digit() || c == b'-' => {
                    let neg = c == b'-';
                    let mut q = p + usize::from(neg);
                    let mut n: i64 = 0;
                    while q < flags.len() && flags[q].is_ascii_digit() {
                        n = n * 10 + i64::from(flags[q] - b'0');
                        q += 1;
                    }
                    off = if neg { -n } else { n };
                    p = q.max(p + 1);
                }
                _ => p += 1,
            }
        }
        if right {
            // Line the replacement up with the old leader's last non-blank, blanking the rest.
            let mut endp = leader.len();
            while endp > 0 && white(leader[endp - 1]) {
                endp -= 1;
            }
            let start = endp.saturating_sub(repl.len());
            let mut new = vec![b' '; start];
            for (i, &c) in leader[..start].iter().enumerate() {
                if white(c) {
                    new[i] = c;
                }
            }
            new.extend_from_slice(&repl);
            new.extend_from_slice(&leader[endp..]);
            leader = new;
        } else {
            // Left adjusted: the replacement overwrites the leader's start, the rest becomes blanks (a
            // blank before a tab is dropped, so the indent stays the same).
            let p0 = skipwhite(&leader, 0);
            let rest_len = leader.len() - p0;
            let mut i = 0;
            while i < rest_len && i < repl.len() {
                i += 1;
            }
            let mut new = leader[..p0].to_vec();
            new.extend_from_slice(&repl);
            let tail = &leader[p0 + i.min(rest_len)..];
            let mut t = 0;
            while t < tail.len() {
                let c = tail[t];
                if white(c) {
                    new.push(c);
                } else if tail.get(t + 1) != Some(&b'\t') {
                    new.push(b' ');
                }
                t += 1;
            }
            leader = new;
        }
        if indent_on {
            let mut ni = indent_cols(&leader, ts).0 as i64;
            if ni + off < 0 {
                off = -ni;
                ni = 0;
            } else {
                ni += off;
            }
            out.newindent = Some(usize::try_from(ni).unwrap_or(0));
            // Correct trailing spaces for the shift, so the alignment stays.
            let has_tab = leader[skipwhite(&leader, 0)..].contains(&b'\t');
            while off > 0 && leader.last() == Some(&b' ') && !has_tab {
                leader.pop();
                off -= 1;
            }
        }
    }
    if leader.last().copied().is_some_and(white) {
        extra_space = false;
    }
    if extra_space {
        leader.push(b' ');
    }
    out.leader = leader;
    out
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
}

/// One step of a cluster walk over `line`: the byte after the cluster at `i` and its cells at `v`.
fn step(line: &[u8], i: usize, v: usize, ts: usize) -> (usize, usize) {
    cluster_cells(line, i, line.len(), v, ts)
}

/// The cells `line[..col]` spans.
fn vcol(line: &[u8], col: usize, ts: usize) -> usize {
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
        i = step(line, i, 0, 8).0;
    }
    last
}

/// The start of the cluster after the one at `col`.
fn next_col(line: &[u8], col: usize) -> usize {
    if col >= line.len() {
        col
    } else {
        step(line, col, 0, 8).0
    }
}

/// The byte of the cluster covering cell `want` (Insert-mode `coladvance`: past the end is the line end).
fn col_at_cell(line: &[u8], want: usize, ts: usize) -> usize {
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

fn char_at(line: &[u8], col: usize) -> Option<char> {
    std::str::from_utf8(&line[col.min(line.len())..])
        .ok()
        .and_then(|s| s.chars().next())
        .or_else(|| line.get(col).map(|&c| c as char))
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
        self.line(l)
            .map_or(0, |s| indent_cols(s, o.ind.tab_width).0)
    }

    /// nvim's `set_indent(cols, SIN_CHANGED)` on line `l`, moving the cursor and the kept position.
    fn set_indent(&mut self, l: usize, cols: usize, o: &FmtOpts<'_>) {
        let (_, old_len) = indent_cols(&self.lines[l], o.ind.tab_width);
        let new = o.ind.rebuilt(&self.lines[l][..old_len], cols);
        if new[..] == self.lines[l][..old_len] {
            return;
        }
        let new_len = new.len();
        self.lines[l].splice(..old_len, new);
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
            let s = std::str::from_utf8(first).unwrap_or("");
            let mut it = s.chars().rev();
            let mut end1 = it.next().unwrap_or('\0');
            let end2 = it.next().unwrap_or('\0');
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
        let ts = o.ind.tab_width;
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
                return Some(indent_cols(line, o.ind.tab_width).0);
            }
            if ll == 0 {
                return self
                    .before
                    .as_deref()
                    .filter(|b| b.windows(2).any(|w| w == b"/*"))
                    .map(|b| indent_cols(b, o.ind.tab_width).0);
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
        let s = std::str::from_utf8(&line[lead..]).ok()?;
        let m = re.find_at(s, 0)?;
        let end = lead + m.end;
        if end >= line.len() {
            return None;
        }
        Some(vcol(line, end, o.ind.tab_width))
    }

    /// Break the cursor line until it fits `tw` (nvim's `internal_format`). `c` is the character about to
    /// be typed at the cursor (`None` when only formatting, `gq`). `format` is `INSCHAR_FORMAT`;
    /// `do_com` / `com_list` are `INSCHAR_DO_COM` / `INSCHAR_COM_LIST`. `insstart_long` is `'fo'` `l`'s
    /// "the line was too long when this insert started".
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn internal_format(
        &mut self,
        tw: usize,
        mut second_indent: i32,
        format: bool,
        mut do_com: bool,
        com_list: bool,
        c: Option<char>,
        o: &FmtOpts<'_>,
    ) {
        let ts = o.ind.tab_width;
        let mut first_line = true;
        let mut no_leader = false;
        // With 'noautoindent' a blank under the cursor is not a break point: mark it while breaking.
        let mut save_char = None;
        if !o.ind.autoindent {
            let (l, col) = self.cur;
            if let Some(&ch) = self.lines[l].get(col) {
                if white(ch) {
                    save_char = Some(ch);
                    self.lines[l][col] = b'x';
                }
            }
        }
        loop {
            let (l, startcol0) = self.cur;
            let line = &self.lines[l];
            let ch_cells = |ch: char| {
                let mut tmp = [0u8; 4];
                let s = ch.encode_utf8(&mut tmp);
                cluster_cells(s.as_bytes(), 0, s.len(), vcol(line, startcol0, ts), ts).1
            };
            let under = c.or_else(|| char_at(line, startcol0));
            let cells = under.map_or(1, ch_cells);
            if vcol(line, startcol0, ts) + cells <= tw {
                break;
            }
            if no_leader {
                do_com = false;
            } else if !format && o.has('c') {
                do_com = true;
            }
            let leader_len = if do_com {
                leader_len(line, &o.parts, false, true).0
            } else {
                0
            };
            if leader_len == 0 {
                no_leader = true;
            }
            if !format && leader_len == 0 && !o.has('t') {
                break;
            }
            let startcol = startcol0;
            if startcol == 0 {
                break;
            }
            let wantcol = col_at_cell(line, tw, ts);
            let mut cur = startcol;
            let mut foundcol = 0usize;
            loop {
                let cc = if cur == startcol && c.is_some() {
                    c
                } else {
                    char_at(line, cur)
                };
                let is_white = |cc: Option<char>| matches!(cc, Some(' ' | '\t'));
                if is_white(cc) {
                    let mut cc = cc;
                    let mut wcc = 0;
                    while cur > 0 && is_white(cc) {
                        cur = prev_col(line, cur);
                        cc = char_at(line, cur);
                        if wcc < 2 {
                            wcc += 1;
                        }
                    }
                    if cur == 0 && is_white(cc) {
                        break; // only blanks before the text
                    }
                    if o.has('p') && cc == Some('.') && wcc < 2 {
                        continue;
                    }
                    if cur < leader_len {
                        break;
                    }
                    if o.has('1') {
                        if cur == 0 || cur <= leader_len {
                            break;
                        }
                        let col = cur;
                        cur = prev_col(line, cur);
                        if is_white(char_at(line, cur)) {
                            continue; // a one-letter word: look further back
                        }
                        cur = col;
                    }
                    cur = next_col(line, cur);
                    foundcol = cur;
                    if cur <= wantcol {
                        break;
                    }
                }
                if cur == 0 {
                    break;
                }
                cur = prev_col(line, cur);
            }
            if foundcol == 0 {
                break;
            }
            // Move the cursor to the break: the blanks there go, the text from `startcol` on keeps its
            // offset from the word start.
            let mut after = foundcol;
            while after < line.len() && white(line[after]) {
                after += 1;
            }
            let keep = startcol.saturating_sub(after);
            self.cur = (l, foundcol);
            let old_indent = 0;
            let did_com = self.open_line(
                do_com,
                com_list,
                if com_list { second_indent } else { old_indent },
                o,
            );
            if did_com {
                no_leader = false;
            }
            if first_line {
                if !com_list {
                    if second_indent < 0 && o.has('n') {
                        second_indent = self
                            .number_indent(l, true, o)
                            .map_or(-1, |n| i32::try_from(n).unwrap_or(i32::MAX));
                    }
                    if second_indent >= 0 {
                        let si = usize::try_from(second_indent).unwrap_or(0);
                        if leader_len > 0 && si > leader_len {
                            let (nl, nc) = self.cur;
                            let pad = si - leader_len;
                            self.lines[nl].splice(nc..nc, std::iter::repeat_n(b' ', pad));
                            shift_col(&mut self.saved, nl, nc, pad as isize);
                            self.cur.1 += pad;
                        } else {
                            let nl = self.cur.0;
                            self.set_indent(nl, si, o);
                        }
                    }
                }
                first_line = false;
            }
            let (nl, nc) = self.cur;
            self.cur = (nl, (nc + keep).min(self.lines[nl].len()));
        }
        if let Some(ch) = save_char {
            let (l, col) = self.cur;
            if col < self.lines[l].len() {
                self.lines[l][col] = ch;
            }
        }
    }

    fn fmt_check_par(
        &self,
        l: usize,
        do_com: bool,
        o: &FmtOpts<'_>,
    ) -> (bool, usize, Option<usize>) {
        let Some(line) = (if l == usize::MAX {
            self.before.as_deref()
        } else {
            self.line(l)
        }) else {
            return (true, 0, None);
        };
        let (len, k) = if do_com {
            leader_len(line, &o.parts, false, true)
        } else {
            (0, None)
        };
        let end_leader = len > 0 && k.is_some_and(|k| is_end_part(&o.parts, k));
        let not_par =
            skipwhite(line, len) >= line.len() || end_leader || line.first() == Some(&0x0c);
        (not_par, len, k)
    }

    /// nvim's `same_leader` for lines `l` and `l + 1`.
    fn same_leader(
        &self,
        l: usize,
        len1: usize,
        k1: Option<usize>,
        len2: usize,
        k2: Option<usize>,
        o: &FmtOpts<'_>,
    ) -> bool {
        if len1 == 0 {
            return len2 == 0;
        }
        let line1 = self.line(l).unwrap_or(&[]);
        if let Some(k) = k1 {
            for f in o.parts[k].flags.bytes() {
                match f {
                    b'f' => return len2 == 0,
                    b'e' => return false,
                    b's' => {
                        if line1.len() <= len1 {
                            return false;
                        }
                        if len2 == 0 {
                            return false;
                        }
                        return k2.is_some_and(|k| o.parts[k].has('m'));
                    }
                    _ => {}
                }
            }
        }
        let line2 = self.line(l + 1).unwrap_or(&[]);
        let mut i1 = skipwhite(line1, 0);
        let mut i2 = 0;
        while i2 < len2 {
            if !white(line2[i2]) {
                if line1.get(i1) != Some(&line2[i2]) {
                    break;
                }
                i1 += 1;
            } else {
                while line1.get(i1).copied().is_some_and(white) {
                    i1 += 1;
                }
            }
            i2 += 1;
        }
        i2 == len2 && i1 == len1
    }

    /// `gq` / `gw` over the buffer's `line_count` lines (nvim's `format_lines`). The cursor starts on
    /// line 0 and ends on the last formatted line.
    pub(crate) fn format_lines(&mut self, line_count: usize, tw: usize, o: &FmtOpts<'_>) {
        let do_com = o.has('q');
        let do_second = o.has('2');
        let do_number = o.has('n');
        let max_len = tw * 3;
        let mut second_indent: i32 = -1;
        let mut do_com_list = false;
        let mut first_par_line = true;
        let mut need_set_indent = true;
        let mut force_format = false;
        let mut next_is_start_par = false;

        let (prev_not_par, _, _) = if self.before.is_some() {
            self.fmt_check_par(usize::MAX, do_com, o)
        } else {
            (true, 0, None)
        };
        let mut is_not_par = prev_not_par;
        let (mut next_is_not_par, mut next_len, mut next_k) = self.fmt_check_par(0, do_com, o);
        let mut is_end_par = is_not_par || next_is_not_par;
        let mut leader = (0usize, None::<usize>);
        let mut advance = true;
        let mut l: isize = -1;
        let mut count = line_count;
        let mut prev_is_end_par = false;
        while count != 0 {
            if advance {
                l += 1;
                prev_is_end_par = is_end_par;
                is_not_par = next_is_not_par;
                leader = (next_len, next_k);
            }
            let lu = usize::try_from(l).unwrap_or(0);
            // nvim walks the cursor down the lines, a skipped one (blank, a comment end) included.
            self.cur.0 = lu;
            let last = lu + 1 >= self.lines.len();
            if count == 1 || last {
                next_is_not_par = true;
                next_len = 0;
                next_k = None;
            } else {
                (next_is_not_par, next_len, next_k) = self.fmt_check_par(lu + 1, do_com, o);
                if do_number {
                    next_is_start_par =
                        self.number_indent(lu + 1, do_com, o).is_some_and(|n| n > 0);
                }
            }
            advance = true;
            is_end_par = is_not_par || next_is_not_par || next_is_start_par;
            if !is_not_par {
                let has_next_buffer_line = lu + 1 < self.lines.len() || self.after.is_some();
                if first_par_line
                    && (do_second || do_number)
                    && prev_is_end_par
                    && has_next_buffer_line
                {
                    let next_empty = self.line(lu + 1).is_none_or(<[u8]>::is_empty);
                    if do_second && !next_empty {
                        if leader.0 == 0 && next_len == 0 {
                            second_indent = i32::try_from(self.get_indent(lu + 1, o)).unwrap_or(0);
                        } else {
                            second_indent = i32::try_from(next_len).unwrap_or(0);
                            do_com_list = true;
                        }
                    } else if do_number {
                        second_indent = self
                            .number_indent(lu, do_com, o)
                            .map_or(-1, |n| i32::try_from(n).unwrap_or(i32::MAX));
                        if leader.0 != 0 || next_len != 0 {
                            do_com_list = true;
                        }
                    }
                }
                if last || !self.same_leader(lu, leader.0, leader.1, next_len, next_k, o) {
                    is_end_par = true;
                }
                if is_end_par || force_format {
                    if need_set_indent {
                        let ind = self.get_indent(lu, o);
                        self.cur = (lu, 0);
                        self.set_indent(lu, ind, o);
                    }
                    // The cursor on the last non-blank.
                    let line = &self.lines[lu];
                    let mut col = if line.is_empty() {
                        0
                    } else {
                        prev_col(line, line.len())
                    };
                    while col > 0 && line.get(col).is_some_and(|c| c.is_ascii_whitespace()) {
                        col = prev_col(line, col);
                    }
                    self.cur = (lu, col);
                    let before = self.lines.len();
                    self.internal_format(
                        tw,
                        second_indent,
                        true,
                        do_com,
                        do_com && do_com_list,
                        None,
                        o,
                    );
                    let grown = self.lines.len() - before;
                    l += isize::try_from(grown).unwrap_or(0);
                    second_indent = -1;
                    need_set_indent = is_end_par;
                    if is_end_par {
                        first_par_line = true;
                    }
                    force_format = false;
                }
                if !is_end_par {
                    advance = false;
                    let lu = usize::try_from(l).unwrap_or(0);
                    if next_len > 0 {
                        self.del_start(lu + 1, next_len);
                    } else if second_indent > 0 {
                        let n = skipwhite(&self.lines[lu + 1], 0);
                        if n > 0 {
                            self.del_start(lu + 1, n);
                        }
                    }
                    self.join2(lu, o);
                    first_par_line = false;
                    force_format = self.lines[lu].len() > max_len;
                }
            }
            count -= 1;
        }
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

/// nvim's `toggle_lines` for `gc`: comment the lines out with `'commentstring'`'s parts, or uncomment
/// them when every non-blank one is commented. `None` when `'commentstring'` has no `%s`.
pub(crate) fn toggle_comment(lines: &[&[u8]], cms: &str) -> Option<Vec<Vec<u8>>> {
    let at = cms.find("%s")?;
    let left = &cms.as_bytes()[..at];
    let right = &cms.as_bytes()[at + 2..];
    let trim = |s: &[u8]| -> Vec<u8> {
        let a = s
            .iter()
            .position(|c| !c.is_ascii_whitespace())
            .unwrap_or(s.len());
        let b = s
            .iter()
            .rposition(|c| !c.is_ascii_whitespace())
            .map_or(a, |p| p + 1);
        s[a..b.max(a)].to_vec()
    };
    let (lt, rt) = (trim(left), trim(right));
    let is_blank = |l: &[u8]| l.iter().all(u8::is_ascii_whitespace);
    // A commented line: blanks, the trimmed left part, anything, the trimmed right part, blanks.
    let commented = |l: &[u8]| {
        let s = l
            .iter()
            .position(|c| !c.is_ascii_whitespace())
            .unwrap_or(l.len());
        let e = l
            .iter()
            .rposition(|c| !c.is_ascii_whitespace())
            .map_or(s, |p| p + 1);
        let body = &l[s..e.max(s)];
        body.len() >= lt.len() + rt.len() && body.starts_with(&lt) && body.ends_with(&rt)
    };
    let mut indent: Option<&[u8]> = None;
    let mut all = true;
    for l in lines {
        let w = l.iter().take_while(|c| c.is_ascii_whitespace()).count();
        if w < l.len() {
            if indent.is_none_or(|i| w < i.len()) {
                indent = Some(&l[..w]);
            }
            if all {
                all = commented(l);
            }
        }
    }
    let indent = indent.unwrap_or(&[]);
    let out = if all {
        lines
            .iter()
            .map(|l| uncomment(l, left, right, &lt, &rt))
            .collect()
    } else {
        lines
            .iter()
            .map(|l| {
                if is_blank(l) {
                    let mut v = indent.to_vec();
                    v.extend_from_slice(&lt);
                    v.extend_from_slice(&rt);
                    v
                } else {
                    let mut v = indent.to_vec();
                    v.extend_from_slice(left);
                    v.extend_from_slice(&l[indent.len().min(l.len())..]);
                    v.extend_from_slice(right);
                    v
                }
            })
            .collect()
    };
    Some(out)
}

/// One line uncommented the way `vim._comment` does (`^(%s*)L(.*)R(%s-)$` with the exact parts, then
/// with the trimmed ones); a line that matches neither stays.
fn uncomment(l: &[u8], left: &[u8], right: &[u8], lt: &[u8], rt: &[u8]) -> Vec<u8> {
    let ws = |c: &u8| c.is_ascii_whitespace();
    let find = |left: &[u8], right: &[u8]| -> Option<(usize, usize, usize, usize)> {
        let lead = l.iter().take_while(|c| ws(c)).count();
        for ind in (0..=lead).rev() {
            if !l[ind..].starts_with(left) {
                continue;
            }
            let bs = ind + left.len();
            for be in (bs..=l.len()).rev() {
                if l[be..].starts_with(right) && l[be + right.len()..].iter().all(ws) {
                    return Some((ind, bs, be, be + right.len()));
                }
            }
        }
        None
    };
    let Some((ind, bs, be, te)) = find(left, right).or_else(|| find(lt, rt)) else {
        return l.to_vec();
    };
    let body = &l[bs..be];
    if body.iter().all(ws) {
        return body.to_vec();
    }
    let mut v = l[..ind].to_vec();
    v.extend_from_slice(body);
    v.extend_from_slice(&l[te..]);
    v
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
