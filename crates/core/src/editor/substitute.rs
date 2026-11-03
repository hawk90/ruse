//! The substitute/global (`:s` / `:g`) data types and the two pure text helpers behind them
//! (`line_spans`, `expand_replacement`). The `EditorState::{substitute, substitute_preview,
//! apply_substitutions, global}` METHODS stay in `mod.rs`'s hub (they read private `View`/`Document`
//! state); they reach these helpers through `mod.rs`'s `pub(crate) use substitute::{…}` re-export.
//! The public types stay `pub` and remain reachable at the crate root via `crate::lib`'s re-export.

use crate::pattern::{Match, Regex};
use crate::pos::BytePos;

/// The pure decision for one command.
#[must_use]
/// The line range a `:s` acts on. `Lines` is 1-based inclusive (as the user types `:N,Ms`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SubRange {
    /// No range prefix: only the cursor's line.
    CurrentLine,
    /// `:%s` — every line.
    WholeFile,
    /// `:N,Ms` — 1-based inclusive line numbers.
    Lines(usize, usize),
}

/// A destination line address for `:m`/`:t` — where to place the moved/copied block. Resolved against the
/// buffer to a 0-based INSERT INDEX in `0..=line_count` (`Line(0)` = top; `Last` = end; `Current` = after
/// the cursor's line). Vim's `:m N` means "after line N", so `Line(n)` inserts before 0-based line `n`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LineAddr {
    /// `:m N` / `:m 0` — after 1-based line N (0 = the very top). Stored as the insert index N.
    Line(usize),
    /// `:m $` — after the last line (the buffer end).
    Last,
    /// `:m .` — after the current (cursor) line.
    Current,
    /// `:m +N` / `:m .-N` — after the line `N` lines below (negative: above) the cursor's line.
    CurrentOffset(isize),
    /// `:m $-N` — after the line `N` lines above the last one.
    LastOffset(isize),
}

impl LineAddr {
    /// The 0-based insert index in `0..=nlines` for a buffer of `nlines` lines with the cursor on 0-based
    /// line `cursor_line`. A relative address that lands outside the buffer is `None` (Vim E16); an
    /// absolute `Line` past the end clamps to the end.
    #[must_use]
    pub fn insert_index(self, cursor_line: usize, nlines: usize) -> Option<usize> {
        let offset =
            |base: usize, d: isize| base.checked_add_signed(d).filter(|&ins| ins <= nlines);
        match self {
            LineAddr::Line(n) => Some(n.min(nlines)),
            LineAddr::Last => Some(nlines),
            LineAddr::Current => Some((cursor_line + 1).min(nlines)),
            LineAddr::CurrentOffset(d) => offset(cursor_line + 1, d),
            LineAddr::LastOffset(d) => offset(nlines, d),
        }
    }
}

/// The `:s///` flags this MVP honors (F-009 #2). `c` (confirm) is an interactive frontend loop, not a
/// field here; `'gdefault'` is applied by the caller (it inverts `g`) before constructing this.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct SubFlags {
    /// `g`: replace ALL matches on each line. Default (no `g`) = the first match per line only.
    pub global: bool,
    /// Case override from a flag: `i` → `Some(true)` (ignore case), `I` → `Some(false)`
    /// (case-sensitive); `None` = use the search config.
    pub ignore_case: Option<bool>,
}

/// One pending substitution from [`crate::EditorState::substitute_preview`]: the byte span `[start, end)` to
/// replace and the (expanded) replacement bytes, plus the 0-based line it sits on. Absolute offsets into
/// the CURRENT buffer — valid only until the buffer is edited, so an interactive confirm loop collects
/// the ACCEPTED subset and applies it all at once ([`crate::EditorState::apply_substitutions`]).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Substitution {
    /// Byte offset of the reported match start (the `\zs`-adjusted start).
    pub start: BytePos,
    /// Byte offset of the reported match end.
    pub end: BytePos,
    /// The replacement bytes to write over `[start, end)`.
    pub replacement: Vec<u8>,
    /// 0-based line index the match sits on (for the "M lines" echo + cursor placement).
    pub line: usize,
}

/// What a `:s` did, for the status echo ("N substitutions on M lines").
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SubOutcome {
    /// Total matches replaced.
    pub replacements: usize,
    /// Distinct lines that had at least one replacement. A multi-line match counts once, on the line it
    /// starts on. `lines > 0` with no replacements is a `:s/\n//` join on the last line: nothing to join,
    /// and not an error (Vim).
    pub lines: usize,
}

/// The command a `:g/pat/cmd` runs on each marked line (F-009 #4). MVP supports the two most common
/// forms; `:normal`, `:m`/`:t`, `:p`, etc. are post-MVP.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum GlobalCmd {
    /// `:g/pat/d` — delete the matched line.
    Delete,
    /// `:g/pat/s/pat2/rep/flags` — substitute on the matched line.
    Substitute {
        /// The substitute pattern (the `:s` pattern, independent of the `:g` selector).
        pattern: String,
        /// The substitute replacement.
        replacement: String,
        /// The substitute flags (`g`/`i`/`I`; `c` confirm is not offered inside `:g`).
        flags: SubFlags,
    },
}

/// Resolve a [`SubRange`] to an inclusive 0-based line index pair `(first, last)` over `lines`, using
/// `cursor_line` for the no-range `CurrentLine` default (as Vim's `:d`/`:m`/`:t`/`:y` do). Both ends are
/// clamped to the last line; the caller guards the empty-buffer case.
pub(crate) fn resolve_line_range(
    range: SubRange,
    lines: &[(usize, usize)],
    cursor_line: usize,
) -> (usize, usize) {
    let last = lines.len().saturating_sub(1);
    match range {
        SubRange::CurrentLine => (cursor_line.min(last), cursor_line.min(last)),
        SubRange::WholeFile => (0, last),
        // A reversed range is taken in order (#733), as the Ex parser already swaps `:3,1d`; an API
        // caller passing one must not underflow.
        SubRange::Lines(a, b) => {
            let (a, b) = (a.min(b), a.max(b));
            (a.saturating_sub(1).min(last), b.saturating_sub(1).min(last))
        }
    }
}

/// The byte span `(start, end)` of every line (`end` excludes the `\n`). Always at least one line. The
/// empty slot after a final newline is not a line in Vim's model (#677), so a terminated buffer has no span
/// for it: `:%d` counts two lines in `a\nb\n`, `:%j` stops at the last real line, and a zero-width pattern
/// never substitutes there. An empty buffer is one empty line.
pub(crate) fn line_spans<B: AsRef<[u8]> + ?Sized>(hay: &B) -> Vec<(usize, usize)> {
    let hay = hay.as_ref();
    let mut out = Vec::new();
    let mut start = 0;
    for (i, &b) in hay.iter().enumerate() {
        if b == b'\n' {
            out.push((start, i));
            start = i + 1;
        }
    }
    if start < hay.len() || out.is_empty() {
        out.push((start, hay.len())); // an unterminated final line, or the empty buffer's one line
    }
    out
}

/// Find the substitutions `:s` makes on the lines `first..=last` (0-based, inclusive) of `hay`, appending
/// them to `out` in document order. `lines` is [`line_spans`] of `hay`. `consumed` carries, across calls,
/// the end of the last match that ran past its own line: a line that match joined away is skipped, as Vim
/// deletes it (`:g/a/s/a\nb/X/` marks lines first, then substitutes on the ones still there).
///
/// A pattern that cannot match a line break is matched one line at a time. A [`Regex::multiline`] pattern
/// is matched against the whole buffer from each line, the way nvim's `do_sub` does (v0.12.6): a match
/// belongs to the line its whole match starts on; after a match that ends on a later line the search goes
/// on in that line (even without `g`, Vim's `do_again`) while it is in the range, and stops once it is
/// past the range. The last line's line break matches `\n` whether or not the buffer ends in one, and
/// the replacement goes in BEFORE it, so the buffer keeps its final newline (`:%s/\n/,/` on `a\nb\n`
/// gives `a,b,\n`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn find_substitutions(
    re: &Regex,
    hay: &str,
    lines: &[(usize, usize)],
    (first, last): (usize, usize),
    global: bool,
    replacement: &str,
    consumed: &mut Option<usize>,
    out: &mut Vec<Substitution>,
) {
    if !re.multiline() {
        for (li, &(ls, le)) in lines.iter().enumerate().take(last + 1).skip(first) {
            // CTRL-C stops the scan; the matches found so far are still substituted, as in nvim.
            if crate::interrupt::breakcheck() {
                return;
            }
            let line = &hay[ls..le];
            let matches: Vec<Match> = if global {
                re.find_all(line)
            } else {
                re.find_at(line, 0).into_iter().collect()
            };
            for m in matches {
                out.push(Substitution {
                    start: BytePos(ls + m.start),
                    end: BytePos(ls + m.end),
                    replacement: expand_replacement(replacement, &line[m.start..m.end]),
                    line: li,
                });
            }
        }
        return;
    }
    // Search a haystack that always ends in a line break, so the last line's `\n` can match. Positions are
    // the buffer's own; an edit never reaches past the buffer (see the clamp below).
    let owned;
    let hay: &str = if hay.ends_with('\n') {
        hay
    } else {
        owned = format!("{hay}\n");
        &owned
    };
    let n = hay.len();
    // The line holding byte `pos`; `lines.len()` for the end of the haystack (past the last line).
    let line_of = |pos: usize| {
        if pos >= n {
            lines.len()
        } else {
            lines.partition_point(|&(s, _)| s <= pos) - 1
        }
    };
    let mut global = global;
    let mut li = first;
    while li <= last {
        if crate::interrupt::breakcheck() {
            return;
        }
        let (ls, le) = lines[li];
        if consumed.is_some_and(|e| ls <= e) {
            li += 1; // joined into the line above by an earlier match
            continue;
        }
        let Some((ws, mut m)) = re.find_whole_at(hay, ls) else {
            return; // no match anywhere below
        };
        if ws > le {
            li = line_of(ws); // the leftmost match starts on a later line: no line before it matches
            continue;
        }
        let group = li;
        let mut cur = li;
        let mut col = ls;
        let mut prev_end: Option<usize> = None;
        loop {
            if m.start >= n {
                return; // `\n\zs` at the end of the last line: past the buffer
            }
            if m.start > lines[cur].1 {
                cur = line_of(m.start); // a `\zs` after a line break reports a later line
            }
            let mut do_again = false;
            let mut stop = false;
            if prev_end == Some(col) && m.start == m.end && m.end == col {
                // An empty match right where the previous one ended does not count (Vim).
                if col >= lines[cur].1 {
                    stop = true;
                } else {
                    col += hay[col..].chars().next().map_or(1, char::len_utf8);
                }
            } else {
                col = m.end;
                prev_end = Some(col);
                // A match through the last line break replaces up to it; the break stays.
                stop = m.end >= n;
                out.push(Substitution {
                    start: BytePos(m.start),
                    end: BytePos(m.end.min(n - 1).max(m.start)),
                    replacement: expand_replacement(replacement, &hay[m.start..m.end]),
                    line: group,
                });
                let end_line = line_of(m.end);
                if end_line > cur {
                    cur = end_line;
                    *consumed = Some(m.end);
                    if cur <= last {
                        do_again = true;
                    } else {
                        global = false;
                    }
                }
            }
            if stop || cur > last || !(global || do_again) {
                break;
            }
            match re.find_whole_at(hay, col) {
                Some((ws, next)) if ws <= lines[cur].1 => m = next,
                _ => break,
            }
        }
        li = cur + 1;
    }
}

/// The active case transform for the replacement expander: uppercase or lowercase.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Case {
    Upper,
    Lower,
}

/// The case-modifier state machine driving [`expand_replacement`]. Vim's `\u`/`\l` set a ONE-CHARACTER
/// pending case that takes priority over any region for the next emitted char, then reverts; `\U`/`\L`
/// open a region that persists until `\e`/`\E`. Setting a region does NOT clear a pending one-char
/// modifier (verified vs nvim: `\l\U&` on "abcd" → "aBCD"), and any emitted char — text OR a control
/// escape like `\r` — consumes the pending modifier (`\u\rx` leaves `x` lowercase).
#[derive(Default)]
struct CaseState {
    /// A one-shot `\u`/`\l` awaiting the next emitted character; wins over `region` for that char.
    pending: Option<Case>,
    /// A `\U`/`\L` region in effect until `\e`/`\E`.
    region: Option<Case>,
}

impl CaseState {
    /// Emit `s` into `out`, applying the pending one-char case to the FIRST char and the region case to
    /// the rest. The pending modifier is consumed by the first char (if any); an empty `s` leaves it.
    fn emit(&mut self, s: &str, out: &mut Vec<u8>) {
        let mut buf = [0u8; 4];
        for ch in s.chars() {
            match self.pending.take().or(self.region) {
                // nvim's `do_upper` / `do_lower`: `mb_toupper` / `mb_tolower`, one character to one
                // (`\U` on `ß` gives `ẞ`, not `SS`).
                Some(Case::Upper) => out.extend_from_slice(
                    crate::casemap::vim_toupper(ch)
                        .encode_utf8(&mut buf)
                        .as_bytes(),
                ),
                Some(Case::Lower) => out.extend_from_slice(
                    crate::casemap::vim_tolower(ch)
                        .encode_utf8(&mut buf)
                        .as_bytes(),
                ),
                None => out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes()),
            }
        }
    }
}

/// Expand a `:s` replacement: `&` / `\0` → the whole matched text; `\t` `\\` `\&` escapes; and the case
/// modifiers `\u` `\l` (next char) / `\U` `\L` (region) / `\e` `\E` (end region), applied to the emitted text
/// (including `&`/`\0` insertions) as a small state machine — see [`CaseState`].
///
/// Line breaks follow Vim (`:help sub-replace-special`, nvim v0.12.6): `\r` and a literal carriage return
/// (typed with `CTRL-V <CR>`) split the line, `\<CR>` inserts a literal carriage return, and `\n` inserts
/// a NUL byte. A carriage return or line break inside the MATCHED text is copied as it is.
///
/// Capture backreferences `\1`-`\9` are DEFERRED: `\(…\)` compiles to a real regex group, but the
/// `pattern::Match` type surfaces only the reported `start`/`end` span, not per-group spans, and the
/// substitute path passes only the whole matched slice here — so group text is not reachable at
/// expansion time without threading group spans through `Match`/`find_all`/`find_at`. An unsupported
/// `\<d>` keeps the digit literal rather than erroring.
pub(crate) fn expand_replacement(replacement: &str, matched: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut st = CaseState::default();
    let mut chars = replacement.chars();
    while let Some(c) = chars.next() {
        match c {
            '&' => st.emit(matched, &mut out),
            '\\' => match chars.next() {
                Some('0') => st.emit(matched, &mut out),
                Some('n') => st.emit("\0", &mut out),
                Some('t') => st.emit("\t", &mut out),
                Some('r') => st.emit("\n", &mut out),
                Some('\r') => st.emit("\r", &mut out),
                Some('&') => st.emit("&", &mut out),
                Some('\\') => st.emit("\\", &mut out),
                Some('u') => st.pending = Some(Case::Upper),
                Some('l') => st.pending = Some(Case::Lower),
                Some('U') => st.region = Some(Case::Upper),
                Some('L') => st.region = Some(Case::Lower),
                Some('e' | 'E') => st.region = None,
                Some(other) => {
                    let mut buf = [0u8; 4];
                    st.emit(other.encode_utf8(&mut buf), &mut out);
                }
                None => st.emit("\\", &mut out),
            },
            '\r' => st.emit("\n", &mut out),
            _ => {
                let mut buf = [0u8; 4];
                st.emit(c.encode_utf8(&mut buf), &mut out);
            }
        }
    }
    out
}
