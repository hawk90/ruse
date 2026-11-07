//! Motions — the geometry half of the editing grammar (C-EDITLANG). A motion resolves, from the cursor, to
//! a target position (a bare move like `w`) or to a byte range (an operator like `dw`). It also owns the
//! shared char-boundary / line helpers used across the editor.
//!
//! Word motions come in two flavors: small-word (`w`/`b`/`e`) split where Vim's character class changes
//! (blank / punctuation / word / emoji / one class per script, [`crate::charclass::vim_class`], #1136), and
//! WORD (`W`/`B`/`E`) split on blanks only. They step one grapheme cluster at a time, as nvim's `inc()` /
//! `dec()` do, and a cluster takes the class of its first char. All positions land on char boundaries.

use crate::cells::Tabs;
use std::cell::Cell;

use unicode_segmentation::{GraphemeCursor, UnicodeSegmentation};

/// A motion in the editing grammar.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Motion {
    /// Vim `h` / `<Left>`: one char left, never past the line's first char (a count stops there).
    Left,
    /// Vim `l` / `<Right>`: one char right, never past the line end. A bare Normal-mode move is pulled back
    /// onto the last char by the planner; Insert may rest after it, and an operator span ends at the newline.
    Right,
    /// Vim `<BS>` with nvim's default `'whichwrap'` `b`: like `h`, but from the first char of a line it
    /// continues on the previous line's last char. Under an operator the newline itself is one step, so
    /// `d<BS>` at column 0 joins the line onto the one above.
    WrapLeft,
    /// Vim `<Space>` with nvim's default `'whichwrap'` `s`: like `l`, but from the last char of a line it
    /// continues on the next line's first char. Under an operator it stays on the line, exactly like `l`.
    WrapRight,
    /// Emacs `backward-char` with a count (`C-u 3 C-b`): one char back per step, the newline included, so
    /// it crosses lines (Emacs point is between chars, D-050).
    EmacsCharBack,
    /// Emacs `forward-char` with a count (`C-u 3 C-f`): one char forward per step, crossing lines.
    EmacsCharFwd,
    Up,
    Down,
    LineStart,
    LineEnd,
    /// Vim `^` / Emacs `back-to-indentation` (M-m): the first non-blank char of the current line (the line
    /// end when the line is all blank). Shared by both profiles — same target, so not a distinct command.
    LineFirstNonBlank,
    /// Vim `g_` — the LAST non-blank char of the line (`{count}g_` = `count-1` lines down). Inclusive
    /// under an operator (`dg_` deletes through the last non-blank).
    LineLastNonBlank,
    /// Vim `|` — go to the `count`-th (1-based) display column of the current line (`5|` = column 5).
    /// Exclusive charwise under an operator. The editor resolves the display column (it knows `'tabstop'`)
    /// and hands this layer the cluster count that reaches it; here `count` steps clusters (#1251).
    Column,
    /// Vim `gM` — go to the char at `count` PERCENT of the line's display width (bare `gM` = 50%; a count
    /// over 100 is 50% too), as nvim measures it. The editor resolves it to a [`Motion::Column`] (#1251); on
    /// its own this layer takes `count` percent of the line's clusters. Exclusive charwise under an
    /// operator (`dgM`), like `|`.
    MidLine,
    /// Small-word motions (Vim `w`/`b`/`e`): three classes — whitespace, word (alnum + `_` + non-ASCII),
    /// punctuation — so `foo.bar` is three words.
    WordFwd,
    WordBack,
    WordEnd,
    /// Vim `ge` — backward to the end of the previous (small-)word. Inclusive under an operator (`dge`).
    WordEndBack,
    /// Emacs `forward-word` / `M-f` (and the span for `kill-word` / `M-d`). Emacs word motions use a TWO-class
    /// syntax split — word constituents (ASCII alphanumerics + non-ASCII) vs everything else (whitespace AND
    /// punctuation AND `_`, all non-word in fundamental-mode) — so it SKIPS a leading non-word run then moves
    /// over the word, landing AFTER the last word char (Emacs point is between-character, D-050). This differs
    /// from Vim's three-class `e`/`WordEnd` (where a punctuation run is its OWN word and `_` is a word char):
    /// `forward-word` on `...foo` reaches the end of `foo`, and on `foo_bar` stops after `foo`. Its operator
    /// span is `[cursor, emacs_word_fwd)`, so `Delete(EmacsWordFwd)` is Emacs `kill-word`.
    EmacsWordFwd,
    /// Emacs `backward-word` / `M-b` (and the span for `backward-kill-word` / `M-DEL`). The mirror of
    /// [`Motion::EmacsWordFwd`]: two-class syntax, so it skips a trailing non-word run backward then moves
    /// over the word to its start. Differs from Vim `b` (`WordBack`) exactly as the forward pair differs:
    /// `backward-word` on `foo.bar` from inside `bar` skips the `.` and lands at the start of `foo`.
    EmacsWordBack,
    /// WORD motions (Vim `W`/`B`/`E`): whitespace-delimited only, so `foo.bar` is one WORD.
    BigWordFwd,
    BigWordBack,
    BigWordEnd,
    /// Vim `gE` — backward to the end of the previous WORD (whitespace-delimited). Inclusive under `dgE`.
    BigWordEndBack,
    /// Text object: the word under the cursor (`iw`) — small-word classes (word / punct / space).
    /// Only meaningful under an operator or in a selection.
    InnerWord,
    /// Text object: the word plus its adjacent whitespace (`aw`). Only meaningful under an operator/selection.
    AWord,
    /// Text object: the WORD under the cursor (`iW`) — whitespace-delimited (`foo.bar` is one WORD).
    InnerBigWord,
    /// Text object: the WORD plus its adjacent whitespace (`aW`).
    ABigWord,
    /// Text object: the paragraph under the cursor (`ip`) — the run of non-blank (or blank) lines. Linewise.
    InnerParagraph,
    /// Text object: the paragraph plus its trailing blank lines, else leading (`ap`). Linewise.
    AParagraph,
    /// Text object: the sentence under the cursor (`is`), excluding trailing whitespace.
    InnerSentence,
    /// Text object: the sentence plus its trailing whitespace (`as`).
    ASentence,
    /// Text object: a matching delimiter pair. `around` includes the delimiters (`a(`), else the inside
    /// (`i(`). Nesting-aware; the pair may span lines. Covers `()`, `{}`, `[]`, `<>`.
    Pair {
        open: char,
        close: char,
        around: bool,
    },
    /// Text object: a quote pair on the current line. `around` includes the quotes and adjacent whitespace
    /// (`a"`), else just the inside (`i"`). Single-line (Vim). Covers `"`, `'`, `` ` ``.
    Quote {
        ch: char,
        around: bool,
    },
    /// Text object: a matching HTML/XML tag block (`it`/`at`). `around` includes the `<tag>`/`</tag>`
    /// delimiters (`at`), else just the content between them (`it`). Nesting-aware; may span lines.
    Tag {
        around: bool,
    },
    /// Linewise (only meaningful under an operator: `dd` / `cc`).
    Line,
    /// Char-search within the current line: `f`/`F` land on the `count`-th `ch`; `t`/`T` stop one char
    /// short of it. `forward` picks the direction, `till` picks find-vs-till. Repeated by `;`/`,`, which set
    /// `repeat`: a repeated `t`/`T` with count 1 skips a match right next to the cursor, so `;` advances
    /// past the char the last `t` stopped before (Vim without `cpo-;`, #802).
    FindChar {
        ch: char,
        forward: bool,
        till: bool,
        repeat: bool,
    },
    /// Go to the `count`-th line (1-based), landing on its first non-blank char (`gg`, `{count}G`). Linewise
    /// under an operator.
    GotoLine,
    /// Go to the last line's first non-blank char (bare `G`). Linewise under an operator.
    LastLine,
    /// Vim `+` / `<CR>` — `count` lines DOWN, landing on the first non-blank char (`^` on the target
    /// line). Linewise under an operator (`d+` deletes this line and the next). Clamps at the last line.
    DownFirstNonBlank,
    /// Vim `-` — `count` lines UP, landing on the first non-blank char. Linewise under an operator
    /// (`d-` deletes this line and the one above). Clamps at the first line.
    UpFirstNonBlank,
    /// Vim `_` — `count-1` lines DOWN, landing on the first non-blank char (`_` alone == `^`, `2_` == one
    /// line down). Linewise under an operator (`d_` == `dd`, `2d_` deletes two lines). Clamps at the last line.
    LineUnderscore,
    /// Vim `[count]go` — go to the `count`-th BYTE of the buffer (1-based; bare `go` = byte 1), snapped to a
    /// char boundary. Exclusive charwise under an operator (`dgo`). `count` past the end clamps to the last byte.
    GotoByte,
    /// Jump to the matching bracket of `()`, `[]`, or `{}` (`%`). Nesting-aware; the match may be on another
    /// line. If the cursor is not on a bracket, the first bracket forward on the line is matched instead.
    MatchBracket,
    /// Vim `{count}%` — jump to `count` PERCENT of the file: line `(count * line_count + 99) / 100`
    /// (1-based), landing on its first non-blank. Linewise under an operator. `count` is the percentage
    /// (not a repeat); bare `%` with no count is [`Motion::MatchBracket`], resolved by the input engine.
    GotoPercent,
    /// Paragraph motion forward (`}`): to the start of the next blank line (or end of buffer). Exclusive
    /// charwise as a bare move; under an operator Vim's exclusive-linewise rule can make it linewise (`d}`).
    ParagraphFwd,
    /// Paragraph motion backward (`{`): to the start of the previous blank line (or start of buffer).
    ParagraphBack,
    /// Sentence motion forward (`)`): to the start of the next sentence (exclusive charwise).
    SentenceFwd,
    /// Sentence motion backward (`(`): to the start of the current/previous sentence (exclusive charwise).
    SentenceBack,
    /// Vim `]]` — forward to the start of the NEXT section: the next line whose first column is `{` (or a
    /// form-feed `\x0C`). Lands on that line's first non-blank (which, for a `{`/form-feed line, is column 0).
    /// With no further section it clamps to the last content line. Exclusive; under an operator Vim's
    /// exclusive-linewise rule makes `d]]`/`y]]` linewise (shared with `ParagraphFwd` in `editor::op_span`).
    SectionFwd,
    /// Vim `[[` — backward to the start of the PREVIOUS section (a `{`/form-feed line in column 0). Clamps to
    /// the first line. Exclusive; backward span is `[target, cursor)`.
    SectionBack,
    /// Vim `][` — forward to the NEXT section END: the next line whose first column is `}` (or a form-feed).
    /// Same wise handling as [`Motion::SectionFwd`].
    SectionEndFwd,
    /// Vim `[]` — backward to the PREVIOUS section end (a `}`/form-feed line in column 0). Clamps to the first
    /// line. Backward span is `[target, cursor)`.
    SectionEndBack,
    /// Vim `[(` — go to the `count`-th previous UNMATCHED `(` (the open paren enclosing the cursor; `count`
    /// steps out `count` nesting levels). Nesting-aware: balanced `()` pairs are skipped while scanning left,
    /// other bracket types are ignored. EXCLUSIVE charwise under an operator — backward span `[target, cursor)`,
    /// which then takes Vim's exclusive-linewise reduction (shared with `d}`/`d[[`). No-op when the cursor is
    /// not inside a `(`.
    UnmatchedParenBack,
    /// Vim `])` — go to the `count`-th next UNMATCHED `)` (the close paren enclosing the cursor). The forward
    /// mirror of [`Motion::UnmatchedParenBack`]: EXCLUSIVE charwise forward span `[cursor, target)`, also
    /// subject to the exclusive-linewise reduction. No-op when the cursor is not inside a `)`.
    UnmatchedParenFwd,
    /// Vim `[{` — go to the `count`-th previous unmatched `{`. As [`Motion::UnmatchedParenBack`] but for braces.
    UnmatchedBraceBack,
    /// Vim `]}` — go to the `count`-th next unmatched `}`. As [`Motion::UnmatchedParenFwd`] but for braces.
    UnmatchedBraceFwd,
    /// Vim `]m` — go to the `count`-th next START of a method (brace block). Pure brace navigation
    /// (ruse has no language model): a faithful port of Vim's `nv_bracket_block` two-phase algorithm.
    /// EXCLUSIVE charwise under an operator (`d]m`), sharing the exclusive-linewise reduction with the
    /// section / unmatched-brace motions. See [`method_target`] for the exact algorithm and the
    /// string/comment divergence.
    MethodStartFwd,
    /// Vim `[m` — go to the `count`-th previous method start. The backward mirror of [`Motion::MethodStartFwd`].
    MethodStartBack,
    /// Vim `]M` — go to the `count`-th next method END (the matching `}`). As [`Motion::MethodStartFwd`].
    MethodEndFwd,
    /// Vim `[M` — go to the `count`-th previous method end. The backward mirror of [`Motion::MethodEndFwd`].
    MethodEndBack,
}

// --- shared byte / char-boundary / line helpers (one home; editor.rs reuses these) ---

/// The length of the UTF-8 char that starts at `i`, or `None` when the bytes there are not one: an invalid
/// lead, a stray continuation byte, a cut, overlong or surrogate sequence (#1232).
pub(crate) fn char_len_at(b: &[u8], i: usize) -> Option<usize> {
    let lead = *b.get(i)?;
    // The lead's length and the range its second byte must fall in (no overlong form, no surrogate, nothing
    // past U+10FFFF), as `std::str::from_utf8` checks.
    let (w, lo, hi) = match lead {
        0x00..=0x7F => return Some(1),
        0xC2..=0xDF => (2, 0x80, 0xBF),
        0xE0 => (3, 0xA0, 0xBF),
        0xED => (3, 0x80, 0x9F),
        0xE1..=0xEF => (3, 0x80, 0xBF),
        0xF0 => (4, 0x90, 0xBF),
        0xF4 => (4, 0x80, 0x8F),
        0xF1..=0xF3 => (4, 0x80, 0xBF),
        _ => return None,
    };
    let rest = b.get(i + 1..i + w)?;
    let ok = (lo..=hi).contains(&rest[0]) && rest[1..].iter().all(|&c| c & 0xC0 == 0x80);
    ok.then_some(w)
}

/// Whether `i` is on a continuation byte of a valid char (strictly inside it). A continuation byte that no
/// valid char owns is a cell of its own, as in nvim (#1232). Looks at most 3 bytes back.
fn inside_char(b: &[u8], i: usize) -> bool {
    if i == 0 || i >= b.len() || b[i] & 0xC0 != 0x80 {
        return false;
    }
    (i.saturating_sub(3)..i)
        .rev()
        .find(|&j| b[j] & 0xC0 != 0x80)
        .is_some_and(|j| char_len_at(b, j).is_some_and(|w| j + w > i))
}

/// Whether `i` is a char boundary: not strictly inside a valid UTF-8 char. A byte that belongs to no valid
/// char is a char of its own (#1232), as nvim counts it; on valid text this is the lead-byte test.
pub(crate) fn is_boundary(b: &[u8], i: usize) -> bool {
    i == 0 || i == b.len() || !inside_char(b, i)
}

pub(crate) fn prev_boundary(b: &[u8], pos: usize) -> usize {
    let mut i = pos.min(b.len());
    while i > 0 {
        i -= 1;
        if is_boundary(b, i) {
            return i;
        }
    }
    0
}

pub(crate) fn next_boundary(b: &[u8], pos: usize) -> usize {
    let mut i = (pos + 1).min(b.len());
    while i < b.len() && !is_boundary(b, i) {
        i += 1;
    }
    i
}

/// The byte offset of the next **grapheme cluster** boundary after `pos` (F-002): one `l`/`MoveRight`
/// step crosses a whole user-perceived character — an emoji ZWJ sequence, a base+combining mark, a
/// wide CJK glyph — not a single `char`, so the cursor never desyncs from the logical text (UAX#29).
/// A byte that is not UTF-8 is a cell of its own, and the valid text around it keeps its clusters (#1232).
///
/// A call costs the cluster it crosses ([`Steps`]).
pub(crate) fn next_grapheme(b: &[u8], pos: usize) -> usize {
    Steps::new(b).next(pos)
}

/// The byte offset of the previous grapheme-cluster boundary before `pos` (the `h`/`MoveLeft` step).
pub(crate) fn prev_grapheme(b: &[u8], pos: usize) -> usize {
    Steps::new(b).prev(pos)
}

/// How many bytes either side of a position a cluster step reads first; doubled while the segmenter asks
/// for more (a long ZWJ sequence, a run of flags, many combining marks).
const STEP_WINDOW: usize = 16;

/// A grapheme-cluster stepper over `b`, with the bounds of the line it last asked about.
///
/// A step reads only the text around `pos` (#1224, #1232). The cells of a line are its valid UTF-8 runs,
/// each segmented into extended grapheme clusters (UAX#29) as if it stood alone, and the bytes that belong
/// to no valid char, one cell each (nvim's `utfc_ptr2len` gives such a byte length 1, and a combining mark
/// after it does not join it). A run ends at a `\n` (a cluster never crosses a line break), at a byte that
/// is not UTF-8, or at the buffer's ends; the segmenter is told so, and is given a window of the run that
/// grows only while it asks for more context ([`GraphemeCursor`]'s chunk protocol). At a line's end (its
/// `\n`) or start a step crosses the newline by the char-boundary fallback, and a position strictly inside
/// a valid char steps to the next or previous char boundary.
///
/// The line-bounds cache is in a `Cell` so a shared `&Steps` (the text objects' buffer view) can use it.
pub(crate) struct Steps<'a> {
    b: &'a [u8],
    /// The cached line `[ls, le]`, `le` being its `\n` or the buffer end; empty (`ls > le`) before first use.
    span: Cell<(usize, usize)>,
}

impl<'a> Steps<'a> {
    pub(crate) fn new(b: &'a [u8]) -> Self {
        Steps {
            b,
            span: Cell::new((1, 0)),
        }
    }

    /// The line holding `pos` (clamped to the buffer) as `(start, end)`, from the cache when `pos` is on
    /// the cached line: [`line_start`] and [`line_end`] give the same line for every offset in `[ls, le]`.
    fn line(&self, pos: usize) -> (usize, usize) {
        let p = pos.min(self.b.len());
        let (ls, le) = self.span.get();
        if ls <= p && p <= le {
            return (ls, le);
        }
        let span = (line_start(self.b, p), line_end(self.b, p));
        self.span.set(span);
        span
    }

    /// [`line_start`] of `pos`.
    pub(crate) fn line_start(&self, pos: usize) -> usize {
        self.line(pos).0
    }

    /// [`line_end`] of `pos`.
    pub(crate) fn line_end(&self, pos: usize) -> usize {
        self.line(pos).1
    }

    /// [`next_grapheme`] of `pos`.
    pub(crate) fn next(&self, pos: usize) -> usize {
        // Two ASCII bytes always have a cluster boundary between them inside a line (no ASCII char extends,
        // prepends or joins; the CR LF pair spans the line end, which is a boundary anyway).
        let b = self.b;
        if pos < b.len() && b[pos] < 0x80 && b.get(pos + 1).is_none_or(|&c| c < 0x80) {
            return pos + 1;
        }
        // The buffer's end, a line's NUL slot, or inside a char: the char step.
        if pos >= b.len() || b[pos] == b'\n' || inside_char(b, pos) {
            return next_boundary(b, pos);
        }
        // A byte that is not UTF-8 is a cell of its own.
        let Some(w) = char_len_at(b, pos) else {
            return pos + 1;
        };
        // A char followed by ASCII (or the line's end): no ASCII char extends, joins or is joined, so the
        // pair alone decides; only a Prepend char would take the ASCII char, which the full step handles.
        let after = pos + w;
        // The run ends after it (the line's end, a byte that is not UTF-8): a cluster ends there.
        if at_nul(b, after) || char_len_at(b, after).is_none() {
            return after;
        }
        // An ASCII char followed by a non-ASCII one: an ASCII char takes part in no rule that looks further
        // back (it is no Prepend, ZWJ, regional indicator or InCB linker), so the pair alone decides whether
        // the next char joins it; a join takes the full step.
        if w == 1 {
            if let Some(d) = char_len_at(b, after) {
                if let Ok(s) = std::str::from_utf8(&b[pos..after + d]) {
                    if let Ok(Some(1)) = GraphemeCursor::new(0, s.len(), true).next_boundary(s, 0) {
                        return after;
                    }
                }
            }
        }
        if b[after] < 0x80 {
            if let Ok(s) = std::str::from_utf8(&b[pos..=after]) {
                if let Ok(Some(r)) = GraphemeCursor::new(0, s.len(), true).next_boundary(s, 0) {
                    if r == w {
                        return after;
                    }
                }
            }
        }
        cluster_step(b, pos, true)
    }

    /// [`prev_grapheme`] of `pos`.
    pub(crate) fn prev(&self, pos: usize) -> usize {
        // The mirror of the ASCII step in [`Steps::next`]: an ASCII byte before `pos` whose own predecessor
        // is ASCII, a newline or the buffer start begins a cluster of its own (a non-ASCII predecessor could
        // be a Prepend char, so that takes the full path).
        let b = self.b;
        if pos > 0
            && pos <= b.len()
            && b[pos - 1] < 0x80
            && (pos == 1 || b[pos - 2] < 0x80 || b[pos - 1] == b'\n')
        {
            return pos - 1;
        }
        let pos = pos.min(b.len());
        // The buffer's start, a line's start (the step crosses back over its `\n`), or inside a char.
        if pos == 0 || b[pos - 1] == b'\n' || inside_char(b, pos) {
            return prev_boundary(b, pos);
        }
        let q = prev_boundary(b, pos);
        // The cell before is a byte that is not UTF-8.
        if char_len_at(b, q).is_none() {
            return q;
        }
        // An ASCII char before `pos` after a non-ASCII one: it extends nothing, so it starts a cluster unless
        // the char before it is a Prepend char, which the pair alone tells.
        if q + 1 == pos && !at_bol(b, q) {
            let p = prev_boundary(b, q);
            if let Some(s) = char_len_at(b, p).and_then(|_| std::str::from_utf8(&b[p..pos]).ok()) {
                let mut gc = GraphemeCursor::new(s.len(), s.len(), true);
                if let Ok(Some(r)) = gc.prev_boundary(s, 0) {
                    if p + r == q {
                        return q;
                    }
                }
            }
        }
        // The char before `pos` starts its run (a line's start, after a byte that is not UTF-8), or has
        // ASCII before it that nothing before can join (its own predecessor is ASCII or the line's start):
        // the text from there to `pos` alone decides.
        let start = if at_bol(b, q) || char_len_at(b, prev_boundary(b, q)).is_none() {
            Some(q)
        } else if b[q - 1] < 0x80 && (at_bol(b, q - 1) || b[q - 2] < 0x80) {
            Some(q - 1)
        } else {
            None
        };
        if let Some(start) = start {
            if let Ok(s) = std::str::from_utf8(&b[start..pos]) {
                let mut gc = GraphemeCursor::new(s.len(), s.len(), true);
                if let Ok(Some(r)) = gc.prev_boundary(s, 0) {
                    return start + r;
                }
            }
        }
        cluster_step(b, pos, false)
    }
}

/// The valid UTF-8 run holding `pos`, as far as `limit` bytes either side: `(start, start_known, text,
/// end_known)`. A side is known when it is the run's real edge (a buffer end, a `\n`, a byte that is not
/// UTF-8) rather than where the window stopped. `pos` is a char boundary inside the run or at its end.
fn utf8_run(b: &[u8], pos: usize, limit: usize) -> (usize, bool, &str, bool) {
    let lo = pos.saturating_sub(limit);
    let hi = (pos + limit).min(b.len());
    let (mut c, mut start_known) = match b[lo..pos].iter().rposition(|&x| x == b'\n') {
        Some(i) => (lo + i + 1, true),
        None => (lo, lo == 0),
    };
    let (e, mut end_known) = match b[pos..hi].iter().position(|&x| x == b'\n') {
        Some(i) => (pos + i, true),
        None => (hi, hi == b.len()),
    };
    if !start_known {
        // The window may cut a char: start at the next char's first byte.
        while c < pos && b[c] & 0xC0 == 0x80 {
            c += 1;
        }
    }
    loop {
        match std::str::from_utf8(&b[c..e]) {
            Ok(s) => return (c, start_known, s, end_known),
            Err(err) => {
                let bad = c + err.valid_up_to();
                let cut = err.error_len().is_none() && !end_known;
                if bad >= pos {
                    // The first fault is at or after `pos`: the run ends there, at a byte that is not UTF-8,
                    // or at a char the window's end cut (the run goes on past the window).
                    end_known = !cut;
                    let s = std::str::from_utf8(&b[c..bad]).unwrap_or_default();
                    return (c, start_known, s, end_known);
                }
                // A byte before `pos` that is not UTF-8: the run starts after it.
                c = bad + err.error_len().unwrap_or(e - bad);
                start_known = true;
            }
        }
    }
}

/// The cluster boundary after (`forward`) or before `pos`, a char boundary inside a valid run (after a
/// valid char, going back). The segmenter sees a window of the run: where the window's edge is not the
/// run's, the text is said to go on (an offset far from 0, a length far past the window), so it asks for
/// more context instead of breaking there, and the window doubles.
fn cluster_step(b: &[u8], pos: usize, forward: bool) -> usize {
    const FAR: usize = 1 << 40;
    let char_step = || {
        if forward {
            next_boundary(b, pos)
        } else {
            prev_boundary(b, pos)
        }
    };
    let mut limit = STEP_WINDOW;
    loop {
        let (ws, start_known, chunk, end_known) = utf8_run(b, pos, limit);
        let base = if start_known { 0 } else { FAR };
        let len = base + chunk.len() + if end_known { 0 } else { FAR };
        let mut gc = GraphemeCursor::new(base + (pos - ws), len, true);
        let step = if forward {
            gc.next_boundary(chunk, base)
        } else {
            gc.prev_boundary(chunk, base)
        };
        match step {
            Ok(Some(r)) => return ws + (r - base),
            // Unreachable: a valid char lies on the stepped side of `pos`. The char step is the safe answer.
            Ok(None) => return char_step(),
            Err(_) if start_known && end_known => return char_step(),
            Err(_) => limit *= 2,
        }
    }
}

pub(crate) fn snap(b: &[u8], pos: usize) -> usize {
    let p = pos.min(b.len());
    if is_boundary(b, p) {
        p
    } else {
        prev_boundary(b, p)
    }
}

// Line boundaries are the one implementation in `crate::pos`; re-exported here so motion call sites
// (`line_start`/`line_end`) read unchanged.
pub(crate) use crate::pos::raw::{line_end, line_start};

/// The char column of `pos` on the line starting at `start`: how many chars lie before it, a byte that
/// belongs to no valid UTF-8 char counting as one (#1243), as [`at_col`] steps them.
pub(crate) fn col_of(b: &[u8], start: usize, pos: usize) -> usize {
    b[start..pos]
        .utf8_chunks()
        .map(|c| c.valid().chars().count() + c.invalid().len())
        .sum()
}

pub(crate) fn at_col(b: &[u8], start: usize, col: usize) -> usize {
    let end = line_end(b, start);
    let mut i = start;
    for _ in 0..col {
        if i >= end {
            break;
        }
        i = next_boundary(b, i);
    }
    i.min(end)
}

/// The VIRTUAL (display) column of `pos` on its line: the cells before it, a `<Tab>` advancing to the next
/// multiple of `tabstop` and every other cluster taking what the renderer paints it in (a wide char 2, an
/// invalid byte's `<ff>` 4, #1251). `start` is the line start. Used by Virtual Replace (`gR`) to size a
/// tab's on-screen width and by `g CTRL-G`; distinct from [`col_of`], which counts characters.
pub(crate) fn vcol_of(b: &[u8], start: usize, pos: usize, tabstop: Tabs) -> usize {
    let end = pos.min(b.len());
    let le = line_end(b, start);
    let mut v = 0;
    let mut i = start;
    while i < end {
        let (j, w) = crate::editor::cluster_cells(b, i, le.max(end), v, tabstop);
        v += w;
        i = j;
    }
    v
}

fn is_ws(c: u8) -> bool {
    c == b' ' || c == b'\t' || c == b'\n'
}

/// The Vim character class of a byte for word motions. Non-ASCII bytes (`>= 0x80`) count as `Word` so a
/// multibyte identifier (e.g. Hangul) is one word; class changes only ever fall on char boundaries.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Space,
    Word,
    Punct,
}

fn class(c: u8) -> Class {
    if is_ws(c) {
        Class::Space
    } else if c == b'_' || c.is_ascii_alphanumeric() || c >= 0x80 {
        Class::Word
    } else {
        Class::Punct
    }
}

// Vim's word motions walk a line model in which every line ends in a NUL slot, which is the `\n` byte here
// (or the buffer end of an unterminated last line); the phantom slot after a final `\n` is no line. The
// helpers below mirror `inc()`/`dec()`/`cls()` of Vim's search.c over that model, so an empty line is a
// stop of its own (#746) and an operator's last word stops at its line end (#742).

/// Vim `cls()` of the char at byte `p`: 0 for a blank (a line break and the buffer end included), else
/// its Vim class ([`crate::charclass::vim_class`]: 1 punctuation, 2 word, 3 emoji, a per-script value), or 1
/// for any non-blank when `big`. A combining mark or the rest of a cluster is never asked: callers step by
/// grapheme cluster, so `p` is a cluster's first char. A byte that is not UTF-8 has the class of the
/// Latin-1 char of its value ([`crate::charclass::cell_class`], #1243).
pub(crate) fn char_cls(b: &[u8], p: usize, big: bool) -> u32 {
    use crate::charclass::{vim_class, WORD};
    let Some(&lead) = b.get(p) else {
        return 0;
    };
    let class = if lead < 0x80 {
        vim_class(char::from(lead))
    } else {
        crate::charclass::cell_class(b, p).unwrap_or(WORD)
    };
    match class {
        _ if lead == b'\n' => 0,
        0 => 0,
        _ if big => 1,
        k => k,
    }
}

// The word walks below step through one [`Steps`], so a cluster step reads only the text it crosses
// (#1224), and they tell a line's ends by its bytes (a `\n`, the buffer's ends) instead of finding the
// line, so a walk costs what it crosses (#1231).

/// Whether `p` is a line's NUL slot on Vim's line model: its `\n`, or the end of the buffer. The same as
/// `p >= line_end(b, p)`, without the scan.
pub(crate) fn at_nul(b: &[u8], p: usize) -> bool {
    p >= b.len() || b[p] == b'\n'
}

/// Whether `p` starts its line: the same as `p == line_start(b, p)` for `p <= b.len()`, without the scan.
pub(crate) fn at_bol(b: &[u8], p: usize) -> bool {
    p == 0 || (p <= b.len() && b[p - 1] == b'\n')
}

/// Vim `cls()` at `p` on Vim's line model: a line's NUL slot is blank.
fn vcls(s: &Steps, p: usize, big: bool) -> u32 {
    if at_nul(s.b, p) {
        return 0;
    }
    char_cls(s.b, p, big)
}

/// Whether `p` is on an empty line (Vim `col == 0 && LINEEMPTY`).
fn on_empty_line(s: &Steps, p: usize) -> bool {
    at_bol(s.b, p) && at_nul(s.b, p)
}

/// Vim `inc()`: the next position and how it got there (0 within the line, 2 onto the line's NUL slot, 1 to
/// the next line's start), or `None` from the last line's NUL slot.
fn vinc(s: &Steps, p: usize) -> Option<(usize, u8)> {
    if !at_nul(s.b, p) {
        // A cluster never crosses the `\n`, so the step lands inside the line or on its NUL slot.
        let q = s.next(p);
        Some((q, if at_nul(s.b, q) { 2 } else { 0 }))
    } else if p + 1 < s.b.len() {
        Some((p + 1, 1))
    } else {
        None
    }
}

/// Vim `dec()`: the previous position (0 within the line, 1 back onto the previous line's NUL slot), or
/// `None` at the buffer start.
fn vdec(s: &Steps, p: usize) -> Option<(usize, u8)> {
    // `p > line_start(b, p)`: `line_start` clamps `p` to the buffer, so past its end that holds too.
    if p > 0 && (p > s.b.len() || s.b[p - 1] != b'\n') {
        Some((s.prev(p), 0))
    } else if p > 0 {
        Some((p - 1, 1))
    } else {
        None
    }
}

/// Vim `fwd_word` (`w` / `W`), `count` times. `eol` is set under an operator: the LAST word moved over then
/// stops at its line's NUL slot instead of crossing to the next line ("the end of that word becomes the end
/// of the operated text", `:help word`), while earlier counts cross freely (`3dw` takes three words over a
/// line break, #742). Skipping white space stops on an empty line, which counts as a word (#746). A landing
/// on a NUL slot is the end of the motion; a bare move pulls it back onto the last char.
fn word_fwd(s: &Steps, pos: usize, big: bool, count: u32, eol: bool) -> usize {
    let b = s.b;
    let mut p = pos.min(b.len());
    for left in (0..count.max(1)).rev() {
        let stop = |i: u8| i >= 1 && eol && left == 0;
        let sclass = vcls(s, p, big);
        let Some((q, i)) = vinc(s, p) else {
            return p;
        };
        // Whether `p` was on the last line (`line_end(b, p) + 1 >= b.len()`), asked only once `inc` left
        // the line's text, which tells where the line ends without a scan: onto the NUL slot (`i == 2`)
        // it is `q`; from the NUL slot (`i == 1`) it is `p`.
        let last_line = |le: usize| le + 1 >= b.len();
        let crossed = match i {
            2 => last_line(q),
            1 => last_line(p),
            _ => false,
        };
        p = q;
        if crossed || stop(i) {
            return p;
        }
        if sclass != 0 {
            while vcls(s, p, big) == sclass {
                let Some((q, i)) = vinc(s, p) else {
                    return p;
                };
                p = q;
                if stop(i) {
                    return p;
                }
            }
        }
        while vcls(s, p, big) == 0 {
            if on_empty_line(s, p) {
                break;
            }
            let Some((q, i)) = vinc(s, p) else {
                return p;
            };
            p = q;
            if stop(i) {
                return p;
            }
        }
    }
    p
}

/// Start of the next (small-)word / WORD — a bare Vim `w` / `W` step.
fn next_word_start(s: &Steps, pos: usize, big: bool) -> usize {
    word_fwd(s, pos, big, 1, false)
}

/// Start of the current/previous word / WORD — Vim `b` / `B` (`bck_word`). An empty line is a stop (#746).
fn prev_word_start(s: &Steps, pos: usize, big: bool) -> usize {
    let Some((mut p, _)) = vdec(s, pos.min(s.b.len())) else {
        return 0;
    };
    // Skip the white space before the word; an empty line is a word of its own.
    while vcls(s, p, big) == 0 {
        if on_empty_line(s, p) {
            return p;
        }
        match vdec(s, p) {
            Some((q, _)) => p = q,
            None => return p,
        }
    }
    // Back to the start of this word, then one forward over the overshoot.
    let c = vcls(s, p, big);
    while vcls(s, p, big) == c {
        match vdec(s, p) {
            Some((q, _)) => p = q,
            None => return p,
        }
    }
    vinc(s, p).map_or(p, |(q, _)| q)
}

/// The landing of Vim `ge` / `gE` (`bckend_word`): the last char of the previous word, or an empty line
/// on the way (#746). Stays at the buffer start when there is no earlier word.
fn prev_word_end(s: &Steps, pos: usize, big: bool) -> usize {
    let mut p = pos.min(s.b.len());
    let sclass = vcls(s, p, big);
    let Some((q, _)) = vdec(s, p) else {
        return p;
    };
    p = q;
    if sclass != 0 {
        while vcls(s, p, big) == sclass {
            match vdec(s, p) {
                Some((q, _)) => p = q,
                None => return p,
            }
        }
    }
    while vcls(s, p, big) == 0 {
        if on_empty_line(s, p) {
            break;
        }
        match vdec(s, p) {
            Some((q, _)) => p = q,
            None => return p,
        }
    }
    p
}

/// One past the last byte of the current/next word / WORD (the exclusive end for `de` / `dE`). The scan
/// starts at the NEXT character, not the next byte: on a multibyte char the next byte is a continuation byte
/// of the same char, which made `e` re-find the word it sits at the end of (#741); and on a base char with
/// combining marks the next char is a mark of the same cell, which did the same (#909).
pub(crate) fn word_end_excl(b: &[u8], pos: usize, big: bool) -> usize {
    word_end_in(&Steps::new(b), pos, big)
}

/// [`word_end_excl`] through a caller's [`Steps`].
fn word_end_in(s: &Steps, pos: usize, big: bool) -> usize {
    let b = s.b;
    let mut i = s.next(pos.min(b.len()));
    while i < b.len() && char_cls(b, i, big) == 0 {
        i = s.next(i);
    }
    if i >= b.len() {
        return b.len();
    }
    run_end(s, i, big)
}

/// One past the run of clusters from `i` that share the class of the one at `i`.
fn run_end(s: &Steps, mut i: usize, big: bool) -> usize {
    let b = s.b;
    let cw = char_cls(b, i, big);
    while i < b.len() && char_cls(b, i, big) == cw {
        i = s.next(i);
    }
    i
}

/// Whether a byte is an Emacs word constituent in fundamental-mode: ASCII alphanumerics, plus any non-ASCII
/// byte (a multibyte letter is a word char in Emacs too). Crucially `_`, punctuation, and whitespace are all
/// NON-word — this is the two-class split that distinguishes Emacs `forward-word`/`backward-word` from Vim's
/// three-class small-word motions (where punctuation is its own class and `_` joins the word).
fn is_emacs_word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c >= 0x80
}

/// Emacs `forward-word` from `pos`: skip a leading run of non-word bytes, then consume the run of word bytes,
/// landing one past the last (Emacs point is between-character). This is the span end for `kill-word` too.
/// Crosses newlines freely — a newline is just another non-word byte, exactly as Emacs `forward-word` does.
fn emacs_word_fwd(b: &[u8], pos: usize) -> usize {
    let mut i = pos.min(b.len());
    while i < b.len() && !is_emacs_word(b[i]) {
        i += 1;
    }
    while i < b.len() && is_emacs_word(b[i]) {
        i += 1;
    }
    i
}

/// Emacs `backward-word` from `pos`: the mirror of [`emacs_word_fwd`] — skip a trailing run of non-word bytes
/// going left, then consume the run of word bytes, landing on the word's first byte.
fn emacs_word_back(b: &[u8], pos: usize) -> usize {
    let mut i = pos.min(b.len());
    while i > 0 && !is_emacs_word(b[i - 1]) {
        i -= 1;
    }
    while i > 0 && is_emacs_word(b[i - 1]) {
        i -= 1;
    }
    i
}

/// The exclusive end of the word the cursor is ON (Vim `cw`'s target when the cursor is on a non-blank).
/// Unlike [`word_end_excl`] / the `e` motion — which, from a word's LAST char, skips forward to the NEXT
/// word's end — this consumes only the current run of same-class chars from `pos`, so `cw` on the last char
/// of a word changes just that char (Vim: "cw changes up to the end of the word"). Returns `pos` unchanged
/// (caller treats it as no-op / falls back) when `pos` is on whitespace or past the buffer.
pub(crate) fn current_word_end_excl(b: &[u8], pos: usize, big: bool) -> usize {
    if pos >= b.len() {
        return b.len();
    }
    if char_cls(b, pos, big) == 0 {
        return pos; // on blank: not the cw special case — caller uses the ordinary word span
    }
    run_end(&Steps::new(b), pos, big)
}

/// The word-object class of a byte: for `iW`/`aW` (`big`) only whitespace-vs-not matters, so punctuation
/// joins the word; for `iw`/`aw` the three small-word classes stay distinct. Class changes only fall on char
/// boundaries (non-ASCII bytes are all `Word`), so a span built from runs of equal `obj_class` is boundary-safe.
fn obj_class(c: u8, big: bool) -> u8 {
    match class(c) {
        Class::Space => 0,
        Class::Word => 1,
        Class::Punct => {
            if big {
                1
            } else {
                2
            }
        }
    }
}

/// The run of same-object-class bytes containing the cursor — Vim `iw` (small) / `iW` (big).
fn inner_word_span(b: &[u8], cur: usize, big: bool) -> (usize, usize) {
    if b.is_empty() {
        return (0, 0);
    }
    let c = cur.min(b.len() - 1);
    let cls = obj_class(b[c], big);
    let mut s = c;
    while s > 0 && obj_class(b[s - 1], big) == cls {
        s -= 1;
    }
    let mut e = c;
    while e < b.len() && obj_class(b[e], big) == cls {
        e += 1;
    }
    (s, e)
}

/// The keyword span under (or, failing that, forward on the current line from) the cursor — Vim `*`/`#`/
/// `gd` (nvim's `find_ident_at_pos`). A keyword is a run of characters of one Vim class of 2 or more
/// ([`crate::charclass::vim_class`]), so "API를" holds two keywords, "API" and "를", as for nvim (#745).
/// Returns `None` when the current line has no keyword at or after the cursor.
pub(crate) fn word_under_cursor(b: &[u8], cur: usize) -> Option<(usize, usize)> {
    use crate::charclass::{cell_class, WORD};
    if b.is_empty() {
        return None;
    }
    // The chars of the line, a byte that is not UTF-8 one char with its Latin-1 class (#1243).
    let class = |p: usize| {
        if at_nul(b, p) {
            0
        } else {
            cell_class(b, p).unwrap_or(0)
        }
    };
    // 1. The first keyword char at or after the cursor, on its line.
    let mut at = snap(b, cur.min(b.len() - 1));
    while class(at) < WORD {
        if at_nul(b, at) {
            return None;
        }
        at = next_boundary(b, at);
    }
    let this = class(at);
    // 2. Back to the start of the run of that class, 3. on to its end.
    let mut start = at;
    while !at_bol(b, start) && class(prev_boundary(b, start)) == this {
        start = prev_boundary(b, start);
    }
    let mut end = at;
    while !at_nul(b, end) && class(end) == this {
        end = next_boundary(b, end);
    }
    Some((start, end))
}

/// The `<cword>` (`big == false`) or `<cWORD>` (`big == true`) span under the cursor — the buffer text
/// Vim's command-line `c_CTRL-R_CTRL-W` / `c_CTRL-R_CTRL-A` splice at the cmdline caret
/// (`:help c_CTRL-R_CTRL-W`, `:help expand()` `<cword>`/`<cWORD>`). Distinct from [`word_under_cursor`]
/// (Vim `*`): that returns `None` when the line has no keyword, whereas `<cword>` falls back to the
/// non-blank (punctuation) run — so `C-r C-w` on `foo ...` at the first `.` yields `...`, where `*` bells.
///
/// `<cword>`: the keyword (alnum/`_`/non-ASCII run) under the cursor; if the cursor is not on a keyword,
/// the search scans FORWARD on the line for the next keyword char (crossing blanks and punctuation). When
/// the rest of the line has no keyword, it falls back to the non-blank run under/forward of the cursor.
///
/// `<cWORD>`: the whitespace-delimited WORD (maximal non-blank run) containing the cursor; if the cursor
/// is on whitespace, the next such run forward on the line.
///
/// Returns `None` when the rest of the line (from the cursor) is blank. Char-boundary safe (spans are
/// built from equal-class runs). Verified against nvim v0.12.4.
pub(crate) fn cword_under_cursor(b: &[u8], cur: usize, big: bool) -> Option<(usize, usize)> {
    if b.is_empty() {
        return None;
    }
    let i = cur.min(b.len() - 1);
    let le = line_end(b, i);
    // The next non-blank position at or forward of the cursor, within the current line (`None` if the
    // rest of the line is blank). Both variants fall back to this run when there is no keyword.
    let first_nonblank = |from: usize| {
        let mut k = from;
        while k < le && class(b[k]) == Class::Space {
            k += 1;
        }
        (k < le && class(b[k]) != Class::Space).then_some(k)
    };
    if big {
        // `<cWORD>`: skip leading blanks, then the maximal non-blank run (word+punct merge under `big`).
        return first_nonblank(i).map(|k| inner_word_span(b, k, true));
    }
    // `<cword>`: prefer the next keyword char forward on the line (crossing blanks and punctuation).
    // A char's class is its Vim class, a byte that is not UTF-8 the one of its Latin-1 char (#1243, #1252).
    let mut j = i;
    while j < le && char_cls(b, j, false) < crate::charclass::WORD {
        j = next_boundary(b, j);
    }
    if j < le {
        return Some(inner_word_span(b, j, false));
    }
    // No keyword on the rest of the line: fall back to the non-blank (punctuation) run.
    first_nonblank(i).map(|k| inner_word_span(b, k, false))
}

/// The keyword (Word-class run) ending exactly AT the caret — the "base" Vim's insert-mode keyword
/// completion (`i_CTRL-N` / `i_CTRL-P`) matches against. Returns `(start, base)`: `start` is the byte
/// offset where the run begins, `base` its text (empty, with `start == cur`, when the char before the
/// caret is not keyword-class or the caret is at a line/buffer start). A keyword is a run of grapheme
/// clusters of one Vim class of 2 or more ([`crate::charclass::vim_class`]), as nvim's
/// `get_normal_compl_info` reads it, so the base of "API를" is "를" (#1136).
pub(crate) fn keyword_before_cursor(b: &[u8], cur: usize) -> (usize, &[u8]) {
    let cur = cur.min(b.len());
    let steps = Steps::new(b);
    let mut start = cur;
    let mut class = None;
    while !at_bol(b, start) {
        let p = steps.prev(start);
        let k = cluster_class(b, p);
        if k < crate::charclass::WORD || class.is_some_and(|c| c != k) {
            break;
        }
        class = Some(k);
        start = p;
    }
    (start, &b[start..cur])
}

/// The Vim class of the grapheme cluster at `p` (its first char's, a byte that is not UTF-8 by its
/// Latin-1 char, #1243), 0 for a line break or the buffer end.
fn cluster_class(b: &[u8], p: usize) -> u32 {
    if at_nul(b, p) {
        return 0;
    }
    crate::charclass::cell_class(b, p).unwrap_or(0)
}

/// Every keyword in `b` as `(start, text)`, in positional order: runs of grapheme clusters of one Vim
/// class of 2 or more (nvim `find_word_end`); a byte that is not UTF-8 is a cluster of its own with the
/// class of its Latin-1 char (#1243).
fn keyword_runs(b: &[u8]) -> Vec<(usize, &[u8])> {
    let mut runs: Vec<(usize, &[u8])> = Vec::new();
    let mut open: Option<(usize, u32)> = None;
    let mut push = |i: usize, k: u32, open: &mut Option<(usize, u32)>| {
        if let Some((s, c)) = *open {
            if c == k {
                return;
            }
            runs.push((s, &b[s..i]));
            *open = None;
        }
        if k >= crate::charclass::WORD {
            *open = Some((i, k));
        }
    };
    if let Ok(text) = std::str::from_utf8(b) {
        for (i, g) in text.grapheme_indices(true) {
            push(i, keyword_class(g), &mut open);
        }
    } else {
        let steps = Steps::new(b);
        let mut i = 0;
        while i < b.len() {
            push(i, cluster_class(b, i), &mut open);
            i = steps.next(i);
        }
    }
    if let Some((s, _)) = open {
        runs.push((s, &b[s..]));
    }
    runs
}

/// The Vim class of a grapheme cluster (its first char's), 0 for a line break.
fn keyword_class(g: &str) -> u32 {
    match g.chars().next() {
        None | Some('\n') => 0,
        Some(c) => crate::charclass::vim_class(c),
    }
}

/// The current-buffer keyword-completion candidates for `i_CTRL-N` / `i_CTRL-P` (F-003), in Vim's scan
/// order. `base` is [`keyword_before_cursor`]'s text; the result is every keyword in the buffer that
/// STARTS WITH `base`, ordered forward from the caret to end-of-buffer then wrapping from the buffer
/// start back to the caret, with the word under construction (the run at the caret) excluded and later
/// DUPLICATES removed (first occurrence in scan order wins). Matching is case-sensitive (nvim default
/// `noignorecase`; the 'ignorecase'/'infercase' interaction is out of scope for this slice). C-P walks
/// this same list backward — the caller cycles the index, so only one order is built. Verified against
/// nvim v0.12.4.
pub(crate) fn keyword_completions(b: &[u8], cur: usize, base: &[u8]) -> Vec<String> {
    // The run under construction begins here; it is never offered as its own candidate. Only meaningful
    // when `base` is non-empty (an empty base sits after a non-word char, so no run starts at the caret).
    let base_start = cur.saturating_sub(base.len());
    let runs = keyword_runs(b);
    // Forward-from-caret first (runs starting at/after the caret), then wrap from the buffer start.
    let ordered = runs
        .iter()
        .filter(|(s, _)| *s >= cur)
        .chain(runs.iter().filter(|(s, _)| *s < cur));
    let mut out: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<&[u8]> = std::collections::HashSet::new();
    for (s, t) in ordered {
        if !base.is_empty() && *s == base_start {
            continue; // the word being typed — never its own candidate
        }
        if !t.starts_with(base) {
            continue;
        }
        // The candidates travel to the input engine as text: a byte that is not UTF-8 is spelled as its
        // escape char ([`crate::pattern::spell`]), which `CompleteWord` turns back into the byte (#1252).
        if seen.insert(t) {
            out.push(crate::pattern::spell(t).into_owned());
        }
    }
    out
}

/// The end of the leading indent (spaces/tabs) on the byte range `[start, end)` — the first offset whose
/// byte is neither a space nor a tab, clamped to `end` when the whole range is blank. Used by whole-line
/// completion, which IGNORES leading indent on both the typed prefix and every candidate line.
fn indent_end(b: &[u8], start: usize, end: usize) -> usize {
    let mut i = start;
    while i < end && (b[i] == b' ' || b[i] == b'\t') {
        i += 1;
    }
    i
}

/// Insert-mode WHOLE-LINE completion source for `i_CTRL-X CTRL-L` (F-003, current-buffer only), in Vim's
/// scan order. Returns `(base, candidates)`: `base` is the caret line's content AFTER its leading indent,
/// up to the caret (the text being completed); each candidate is another buffer line's content after its
/// OWN leading indent — indent is IGNORED for both the match and the inserted text (`:help i_CTRL-X_CTRL-L`,
/// "Indent is ignored") — that STARTS WITH `base`. The order mirrors Vim's backward search: the line just
/// ABOVE the caret, upward to the top, then WRAPPING from the bottom line upward back to the line just
/// below the caret; the caret's own line is never a candidate, and later DUPLICATES (by post-indent
/// content, so lines differing only in indent collapse) are removed — first in scan order wins. Matching is
/// case-sensitive (nvim default `noignorecase`; the 'ignorecase'/'infercase' interaction is out of scope
/// for this slice). C-P walks the same list backward — the caller cycles the index, so only one order is
/// built. Verified against nvim v0.12.4.
pub(crate) fn line_completions(b: &[u8], cur: usize) -> (String, Vec<String>) {
    // `base`: the caret line's post-indent text up to the caret (empty when the caret sits within indent).
    let ls = crate::pos::raw::line_start(b, cur);
    let bi = indent_end(b, ls, cur);
    // Text travels spelled ([`crate::pattern::spell`]): a byte that is not UTF-8 is its escape char, which
    // `CompleteWord` turns back into the byte (#1252). Matching is on the bytes.
    let base_bytes = &b[bi..cur];
    let base = crate::pattern::spell(base_bytes).into_owned();
    // Line geometry: `n` rows total, the caret on row `cr`. Scan backward from `cr` with wraparound.
    // The slot after a final newline is a row only while the caret is on it (a line being typed); else it
    // is no line and offers no empty candidate (#677).
    let cr = crate::pos::raw::line_of(b, cur);
    let last = crate::pos::raw::line_of(b, b.len());
    let n = if b.last() == Some(&b'\n') && cr < last {
        last
    } else {
        last + 1
    };
    let mut out: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for i in 1..n {
        let row = (cr + n - i) % n; // cr-1, cr-2, …, 0, n-1, …, cr+1 — every row but the caret's
        let rs = crate::pos::raw::nth_line_start(b, row);
        let re = crate::pos::raw::line_end(b, rs);
        let ri = indent_end(b, rs, re);
        let content = &b[ri..re];
        if !content.starts_with(base_bytes) {
            continue;
        }
        let text = crate::pattern::spell(content).into_owned();
        if seen.insert(text.clone()) {
            out.push(text);
        }
    }
    (base, out)
}

/// Whether the line starting at `ls` is blank — empty or (a start pointing at) a newline / end of buffer.
/// Paragraph boundaries in Vim are truly-empty lines (a whitespace-only line is *not* a boundary).
fn is_blank_line(b: &[u8], ls: usize) -> bool {
    ls >= b.len() || b[ls] == b'\n'
}

/// The starts of the buffer's lines in Vim's model (the slot after a final newline is not a line).
fn line_starts(b: &[u8]) -> Vec<usize> {
    let end = content_end(b);
    let mut v = vec![0];
    v.extend(
        b.iter()
            .enumerate()
            .filter(|&(i, &c)| c == b'\n' && i < end)
            .map(|(i, _)| i + 1),
    );
    v
}

/// The 0-based line of `pos` among `starts`.
fn line_of_starts(starts: &[usize], pos: usize) -> usize {
    starts.partition_point(|&s| s <= pos).saturating_sub(1)
}

/// The paragraph object (`ip` / `ap`) with `count`, as Vim's `current_par` resolves it for an operator: the
/// 0-based first and last line, or `None` when it fails (`count` paragraphs do not fit before the end, or
/// `ap` starts on blank lines with no paragraph after them, #1089). A white line holds only blanks; a
/// non-white run also stops at a line starting with a form feed (`startPS`).
fn paragraph_lines(b: &[u8], cur: usize, count: u32, include: bool) -> Option<(usize, usize)> {
    let starts = line_starts(b);
    let last = starts.len() - 1;
    let white = |l: usize| {
        let ls = starts[l];
        b[ls..line_end(b, ls)]
            .iter()
            .all(|&c| c == b' ' || c == b'\t')
    };
    let start_ps = |l: usize| {
        let ls = starts[l];
        ls >= b.len() || b[ls] == b'\n' || b[ls] == 0x0C
    };
    let mut start = line_of_starts(&starts, cur);
    let white_in_front = white(start);
    while start > 0 {
        if white_in_front {
            if !white(start - 1) {
                break;
            }
        } else if white(start - 1) || start_ps(start) {
            break;
        }
        start -= 1;
    }
    // `next` is the first line not taken yet (Vim's `end_lnum + 1`): past the white lines at the start.
    let mut next = start;
    while next <= last && white(next) {
        next += 1;
    }
    let mut i = count.max(1);
    if !include && white_in_front {
        i -= 1;
    }
    while i > 0 {
        i -= 1;
        if next > last {
            return None;
        }
        let do_white = !include && white(next);
        if include || !do_white {
            next += 1;
            while next <= last && !white(next) && !start_ps(next) {
                next += 1;
            }
        }
        if i == 0 && white_in_front && include {
            break;
        }
        if include || do_white {
            while next <= last && white(next) {
                next += 1;
            }
        }
    }
    let end = next - 1;
    // `ap` with no white lines after it takes the ones before it.
    if !white_in_front && !white(end) && include {
        while start > 0 && white(start - 1) {
            start -= 1;
        }
    }
    Some((start, end))
}

/// The paragraph object as a linewise byte range `[s, e)` covering whole lines ([`paragraph_lines`]); a
/// failed object is the empty range at the cursor.
fn paragraph_span(b: &[u8], cur: usize, count: u32, around: bool) -> (usize, usize) {
    if b.is_empty() {
        return (0, 0);
    }
    let Some((first, last)) = paragraph_lines(b, cur, count, around) else {
        return (cur, cur);
    };
    let starts = line_starts(b);
    let le = line_end(b, starts[last]);
    (starts[first], if le < b.len() { le + 1 } else { le })
}

/// Whether the paragraph object fails ([`paragraph_lines`]).
fn paragraph_fails(b: &[u8], cur: usize, count: u32, around: bool) -> bool {
    !b.is_empty() && paragraph_lines(b, cur, count, around).is_none()
}

/// One `}` step (Vim `findpar`): the start of the next blank line after the cursor's line that follows a
/// non-blank one — from a blank line the run of blanks is skipped first (#748) — or `None` when the scan
/// runs off the last line.
fn next_para_boundary(b: &[u8], pos: usize) -> Option<usize> {
    let mut ls = line_start(b, pos);
    let mut did_skip = false;
    loop {
        if !is_blank_line(b, ls) {
            did_skip = true;
        }
        let le = line_end(b, ls);
        if le + 1 >= b.len() {
            return None;
        }
        ls = le + 1;
        if did_skip && is_blank_line(b, ls) {
            return Some(ls);
        }
    }
}

/// One `{` step: the start of the previous blank line before the cursor's line that precedes a non-blank
/// one (the mirror of [`next_para_boundary`]: a run of blanks above the cursor is skipped first), or `None`
/// when the scan runs off the first line.
fn prev_para_boundary(b: &[u8], pos: usize) -> Option<usize> {
    let mut ls = line_start(b, pos);
    let mut did_skip = !is_blank_line(b, ls);
    while ls > 0 {
        let prev_ls = line_start(b, ls - 1);
        if did_skip && is_blank_line(b, prev_ls) {
            return Some(prev_ls);
        }
        if !is_blank_line(b, prev_ls) {
            did_skip = true;
        }
        ls = prev_ls;
    }
    None
}

/// `{count}}` / `{count}{`: `count` paragraph steps. A step running off the buffer lands on its edge —
/// for `}` the LAST CHAR of the last line (never its newline or the phantom slot after it, #677; Vim's `}`
/// turns inclusive there), for `{` the buffer start — but only as the final step: with count left over the
/// motion fails and stays put (Vim `findpar` returns false, so `d2}` over one paragraph is a no-op).
/// Returns the landing and whether it is that end-of-buffer edge, or `None` when the motion fails.
fn paragraph_landing(b: &[u8], cur: usize, forward: bool, n: u32) -> Option<(usize, bool)> {
    let mut c = cur;
    for k in 0..n {
        let step = if forward {
            next_para_boundary(b, c)
        } else {
            prev_para_boundary(b, c)
        };
        match step {
            Some(t) => c = t,
            None if k + 1 < n => return None,
            None if forward => {
                let le = content_end(b); // the last line's end
                let ls = line_start(b, le);
                return Some((if le > ls { prev_grapheme(b, le) } else { le }, true));
            }
            None => return Some((0, true)),
        }
    }
    Some((c, false))
}

fn paragraph_target(b: &[u8], cur: usize, forward: bool, n: u32) -> usize {
    paragraph_landing(b, cur, forward, n).map_or(cur, |(t, _)| t)
}

/// Whether the line starting at `ls` is a SECTION boundary line. A form-feed (`\x0C`) in column 0 is a
/// boundary for both the start (`]]`/`[[`) and end (`][`/`[]`) motions; otherwise the first byte must be the
/// brace for the direction — `{` for section starts, `}` for section ends. (Vim's default: `{`/`}` in the
/// first column plus form-feed; the nroff `sections` macros are a documented follow-up.)
fn is_section_line(b: &[u8], ls: usize, ends: bool) -> bool {
    match b.get(ls) {
        Some(&0x0C) => true,
        Some(&c) => c == if ends { b'}' } else { b'{' },
        None => false,
    }
}

/// The line start of the NEXT section boundary strictly after `ls`, or `None` when the run reaches the last
/// content line without finding one (the caller clamps to the last line, matching nvim's EOF landing).
fn scan_section_fwd(b: &[u8], ls: usize, ends: bool) -> Option<usize> {
    let mut cur = ls;
    loop {
        let le = line_end(b, cur);
        if le >= content_end(b) {
            return None; // `cur` is the last content line — no next line to test
        }
        cur = le + 1;
        if is_section_line(b, cur, ends) {
            return Some(cur);
        }
    }
}

/// The line start of the PREVIOUS section boundary strictly before `ls`, or `None` when none exists (the
/// caller clamps to the first line).
fn scan_section_back(b: &[u8], ls: usize, ends: bool) -> Option<usize> {
    let mut cur = ls;
    while cur > 0 {
        cur = line_start(b, cur - 1);
        if is_section_line(b, cur, ends) {
            return Some(cur);
        }
    }
    None
}

/// The `count`-th section boundary line-start forward (`]]`/`][`), clamping to the last content line when the
/// sections run out (nvim rests the cursor on the last line at that point).
fn next_section_line(b: &[u8], pos: usize, count: u32, ends: bool) -> usize {
    let mut ls = line_start(b, pos);
    for _ in 0..count.max(1) {
        match scan_section_fwd(b, ls, ends) {
            Some(f) => ls = f,
            None => return last_line_start(b),
        }
    }
    ls
}

/// The `count`-th section boundary line-start backward (`[[`/`[]`), clamping to the first line.
fn prev_section_line(b: &[u8], pos: usize, count: u32, ends: bool) -> usize {
    let mut ls = line_start(b, pos);
    for _ in 0..count.max(1) {
        match scan_section_back(b, ls, ends) {
            Some(f) => ls = f,
            None => return 0,
        }
    }
    ls
}

/// The leading tag NAME of a tag's interior bytes (the run after `<` or `</`): ASCII letters/digits and
/// `-`/`_`/`:`, stopping at whitespace, `/`, or end. `<div class=x>` → `div`; `</div>` interior → `div`.
fn tag_name(interior: &[u8]) -> &[u8] {
    let end = interior
        .iter()
        .position(|&c| !(c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b':')))
        .unwrap_or(interior.len());
    &interior[..end]
}

/// The HTML/XML tag block enclosing `cur` (Vim `it`/`at`) — a nesting-aware byte scan (no syntax tree
/// needed, like bracket matching). `around` = `at` (include the `<tag>`/`</tag>` delimiters), else `it`
/// (the content between them). `count` picks how many nesting levels to climb from the innermost enclosing
/// pair: `1` (bare `it`/`at`) is the innermost, `2` (`2it`/`2at`) its enclosing tag, and so on (Vim's
/// count-expands-outward semantics). Returns `(cur, cur)` (a no-op) when the cursor is not inside any tag
/// pair, or when `count` exceeds the enclosing nesting depth (`3dit` on a 2-deep block is a no-op in Vim).
/// The returned offsets sit at ASCII `<`/`>` boundaries, so they are always char-boundary safe.
fn tag_span(b: &[u8], cur: usize, around: bool, count: u32) -> (usize, usize) {
    let n = b.len();
    // Open tags awaiting their close: (name, open_start, open_end-exclusive).
    let mut stack: Vec<(&[u8], usize, usize)> = Vec::new();
    // Every completed pair CONTAINING `cur`, as (open_start, open_end, close_start, close_end). Collected
    // as their closes are found; later sorted innermost-first so `count` can index outward.
    let mut enclosing: Vec<(usize, usize, usize, usize)> = Vec::new();
    let mut i = 0;
    while i < n {
        if b[i] != b'<' {
            i += 1;
            continue;
        }
        let start = i;
        let mut j = i + 1;
        while j < n && b[j] != b'>' {
            j += 1;
        }
        if j >= n {
            break; // unterminated `<…`
        }
        let end = j + 1; // one past the `>`
        let interior = &b[start + 1..j];
        if interior.first() == Some(&b'/') {
            // Close tag `</name>`: pop to the matching open, forming a pair.
            let name = tag_name(&interior[1..]);
            while let Some((oname, os, oe)) = stack.pop() {
                if oname == name {
                    if os <= cur && cur < end {
                        enclosing.push((os, oe, start, end));
                    }
                    break;
                }
                // Mismatched close (malformed markup): keep popping to recover.
            }
        } else if interior.last() != Some(&b'/') {
            // Open tag `<name …>` (a `<name/>` self-close has no content and is skipped).
            let name = tag_name(interior);
            if !name.is_empty() {
                stack.push((name, start, end));
            }
        }
        i = end;
    }
    // Innermost first (smallest span). `count == 1` picks it; each extra count climbs one level out.
    enclosing.sort_by_key(|&(os, _, _, ce)| ce - os);
    match enclosing.get((count.max(1) - 1) as usize) {
        Some(&(os, _, _, ce)) if around => (os, ce),
        Some(&(_, oe, cs, _)) => (oe, cs),
        None => (cur, cur),
    }
}

fn up(b: &[u8], cur: usize) -> usize {
    let ls = line_start(b, cur);
    if ls == 0 {
        cur
    } else {
        at_col(b, line_start(b, ls - 1), col_of(b, ls, cur))
    }
}

/// The byte offset just past the last line that holds content. A buffer ending in `\n` has that newline as
/// the last line's TERMINATOR, not the start of a new empty line (Vim's model), so the trailing (phantom)
/// slot at `b.len()` is excluded. Vertical / `G` / `+`-family motions never step past this — `G` on
/// `"abc\n"` lands on `abc`, not on a blank line below it. Equals `b.len()` when there is no trailing `\n`.
pub(crate) fn content_end(b: &[u8]) -> usize {
    if b.last() == Some(&b'\n') {
        b.len() - 1
    } else {
        b.len()
    }
}

/// Pull an on-char (Vim Normal/Visual) caret off the phantom slot after a buffer's final newline (#677).
/// That slot is not a line in Vim's model, so a cursor left there by a motion running off the end (`w`,
/// `}`, `)`, `g_`, …), a linewise delete of the last line, or an undo would make the next typed text land
/// on an extra unterminated line. It moves onto the last line's last char (its newline when that line is
/// empty), where Vim leaves it. Any other position, and every position of an unterminated buffer, is
/// returned unchanged.
#[must_use]
pub fn off_phantom(b: &[u8], pos: crate::pos::BytePos) -> crate::pos::BytePos {
    crate::pos::BytePos(off_phantom_at(b, pos.0))
}

/// [`off_phantom`] over a bare offset, for the core's own motions.
#[must_use]
pub(crate) fn off_phantom_at(b: &[u8], pos: usize) -> usize {
    if pos < b.len() || b.last() != Some(&b'\n') {
        return pos;
    }
    let le = b.len() - 1;
    if !at_bol(b, le) {
        prev_grapheme(b, le)
    } else {
        le
    }
}

fn down(b: &[u8], cur: usize) -> usize {
    let le = line_end(b, cur);
    // Stop at the last content line: `le >= content_end` covers both a no-newline last line (`le == len`)
    // and a trailing-newline last line (`le == len-1`), so `j` never lands on the phantom empty slot.
    if le >= content_end(b) {
        cur
    } else {
        at_col(b, le + 1, col_of(b, line_start(b, cur), cur))
    }
}

/// Vertical motion (`j`/`k`) that honours a STICKY desired column (`want`, Vim `curswant`) instead of the
/// cursor's current column, so moving through a shorter interior line does not collapse the column: `want`
/// is clamped to each line's length by [`at_col`] but never forgotten. `want == usize::MAX` (set by `$`)
/// rides each line's end. `down` picks the direction; `count` repeats. `want` is a virtual (screen) column,
/// so a `<Tab>` (`ts`) or a wide char counts its cells, as nvim's `curswant` does (#1090).
pub(crate) fn vmove(
    b: &[u8],
    cursor: usize,
    count: u32,
    down: bool,
    want: usize,
    ts: Tabs,
) -> usize {
    let mut c = cursor.min(b.len());
    for _ in 0..count.max(1) {
        if down {
            let le = line_end(b, c);
            if le >= content_end(b) {
                break; // don't descend onto the phantom line after a trailing '\n'
            }
            c = crate::editor::want_col(b, le + 1, want, ts);
        } else {
            let ls = line_start(b, c);
            if ls == 0 {
                break;
            }
            c = crate::editor::want_col(b, line_start(b, ls - 1), want, ts);
        }
    }
    c
}

/// The bare-move landing for a char-search (`f`/`F`/`t`/`T`), or `None` if the `count`-th match does not
/// exist on the current line. `f`/`F` land on the char; `t`/`T` land one boundary short of it. Search is
/// confined to the current line (Vim never crosses a newline for these). A `;`/`,` repeat (`repeat`) of a
/// `t`/`T` with count 1 does not match the char right next to the cursor, which is where the last `t`
/// stopped short of (Vim `searchc` without `cpo-;`).
fn find_char_target(
    b: &[u8],
    cursor: usize,
    ch: char,
    forward: bool,
    till: bool,
    repeat: bool,
    count: u32,
) -> Option<usize> {
    let n = count.max(1) as usize;
    // A target matches the FIRST char of a cluster only: `f` to a combining mark, an emoji modifier or the
    // second flag letter finds nothing inside a cluster, as nvim v0.12.6 (`f\u{301}` on `xáb` fails; #909).
    // The cells are walked from the cursor as the motions step them, so a byte that is not UTF-8 is a cell
    // of its own that matches no char, and the rest of the line is still searched (#1232).
    let steps = Steps::new(b);
    let first_char = |p: usize| {
        char_len_at(b, p)
            .and_then(|w| std::str::from_utf8(&b[p..p + w]).ok())
            .and_then(|s| s.chars().next())
    };
    // `t`/`T` stop one CHARACTER short of the match: a base with its marks, a flag, a ZWJ sequence (#909).
    let skip = (repeat && till && n == 1).then(|| {
        if forward {
            steps.next(cursor)
        } else {
            steps.prev(cursor)
        }
    });
    let is_hit = |p: usize| first_char(p) == Some(ch) && Some(p) != skip;
    let mut left = n;
    let mut p = cursor.min(b.len());
    if forward {
        // Cell starts strictly after the cursor, in order, up to the line's end; take the n-th match.
        while !at_nul(b, p) {
            p = steps.next(p);
            if !at_nul(b, p) && is_hit(p) {
                left -= 1;
                if left == 0 {
                    return Some(if till { steps.prev(p) } else { p });
                }
            }
        }
        None
    } else {
        // Cell starts strictly before the cursor, back to the line's start; take the n-th match.
        while !at_bol(b, p) {
            p = steps.prev(p);
            if is_hit(p) {
                left -= 1;
                if left == 0 {
                    return Some(if till { steps.next(p) } else { p });
                }
            }
        }
        None
    }
}

/// First non-blank (non-space, non-tab) byte position on the line containing `line_start_pos`, or the line
/// start itself if the whole line is blank. Where `gg`/`G` land (Vim).
pub(crate) fn first_non_blank(b: &[u8], line_start_pos: usize) -> usize {
    let ls = line_start(b, line_start_pos);
    let le = line_end(b, ls);
    let mut i = ls;
    while i < le && (b[i] == b' ' || b[i] == b'\t') {
        i = next_boundary(b, i);
    }
    i
}

/// The last non-blank char of the line containing `pos` (Vim `g_`), or the line start when the line is
/// blank/empty. Lands ON the char (like `$`), not past it.
fn last_non_blank(b: &[u8], pos: usize) -> usize {
    let ls = line_start(b, pos);
    let le = line_end(b, ls);
    let mut i = le;
    while i > ls {
        let p = prev_grapheme(b, i);
        if b[p] != b' ' && b[p] != b'\t' {
            return p;
        }
        i = p;
    }
    ls
}

/// Byte start of the `n`-th line (1-based), clamped to the last line.
pub(crate) fn nth_line_start(b: &[u8], n: u32) -> usize {
    let target = n.max(1) - 1; // 0-based
    let mut seen = 0u32;
    let mut ls = 0usize;
    while seen < target {
        let le = line_end(b, ls);
        if le >= content_end(b) {
            break; // no more content lines; clamp to the last real one (never the trailing phantom)
        }
        ls = le + 1;
        seen += 1;
    }
    ls
}

/// Byte start of the last content line (the trailing phantom slot after a final `\n` is not a line).
pub(crate) fn last_line_start(b: &[u8]) -> usize {
    line_start(b, content_end(b))
}

/// The matching-bracket position for `%`, or `None` if there is nothing to match. If the byte at `cursor`
/// is a bracket, its pair is found (nesting-aware, possibly on another line); otherwise the first bracket
/// forward on the current line is matched (Vim). Brackets are ASCII, so byte scanning stays on boundaries.
fn match_bracket(b: &[u8], cursor: usize) -> Option<usize> {
    // nvim's `findmatchlimit` (#1163): quotes, character literals, escapes, comments, `#if` chains and
    // the 'lisp' rules.
    crate::findmatch::percent(b, cursor)
}

/// The `count`-th UNMATCHED bracket of one type from the cursor — Vim `[(`/`])` (parens) and `[{`/`]}`
/// (braces). `forward` scans right from `cursor+1` for an unmatched CLOSE (`])`/`]}`); otherwise it scans
/// left from `cursor-1` for an unmatched OPEN (`[(`/`[{`). Nesting is same-type: a balanced pair is skipped
/// (a `)` seen while scanning left raises the depth so its matching `(` is not counted), and other bracket
/// types are ignored — exactly as Vim's `[(` skips `{}`/`[]`. Returns `cursor` (a no-op) when there is no
/// such unmatched bracket. Brackets are ASCII → the result is always a char boundary.
fn unmatched_bracket(
    b: &[u8],
    cursor: usize,
    open: u8,
    close: u8,
    forward: bool,
    count: u32,
) -> usize {
    let mut remaining = count.max(1);
    let mut depth = 0u32;
    if forward {
        let mut i = cursor.saturating_add(1); // strictly after the cursor (an on-cursor close does not count)
        while i < b.len() {
            if b[i] == open {
                depth += 1;
            } else if b[i] == close {
                if depth == 0 {
                    remaining -= 1;
                    if remaining == 0 {
                        return i;
                    }
                } else {
                    depth -= 1;
                }
            }
            i += 1;
        }
        cursor
    } else {
        let mut i = cursor.min(b.len()); // decremented before each read → starts strictly before the cursor
        while i > 0 {
            i -= 1;
            if b[i] == close {
                depth += 1;
            } else if b[i] == open {
                if depth == 0 {
                    remaining -= 1;
                    if remaining == 0 {
                        return i;
                    }
                } else {
                    depth -= 1;
                }
            }
        }
        cursor
    }
}

/// `findmatchlimit` restricted to the two uses Vim's `nv_bracket_block` makes of it for the method
/// motions: find the single enclosing UNMATCHED brace of the class the cursor is in. `[m`/`[M` search
/// BACKWARD for an unmatched `{`; `]m`/`]M` search FORWARD for an unmatched `}`. Returns `None` when
/// there is no such brace (i.e. `unmatched_bracket` reports a no-op by returning `cursor`).
fn find_enclosing_brace(b: &[u8], cursor: usize, forward: bool) -> Option<usize> {
    let t = unmatched_bracket(b, cursor, b'{', b'}', forward, 1);
    if t == cursor {
        None
    } else {
        Some(t)
    }
}

/// Vim `]m`/`[m`/`]M`/`[M` — the method (brace-block) motions. This is a faithful port of Vim's
/// `nv_bracket_block` (`normal.c`): ruse has NO language model, so the "method" is defined purely by
/// brace structure, exactly as Vim's own algorithm defines it via `findmatchlimit` over `{`/`}`.
///
/// `forward` is true for `]m`/`]M` (search right), false for `[m`/`[M`. `end` is true for `]M`/`[M`
/// (the method END, a `}`), false for `]m`/`[m` (the method START, a `{`).
///
/// The algorithm has two phases, mirroring Vim:
///   * Phase 1 walks OUT of the cursor's nesting to the outermost enclosing brace (`new_pos`), keeping
///     the one level in (`prev_pos`), by repeatedly finding the enclosing unmatched `{` (backward cmds)
///     or `}` (forward cmds).
///   * Phase 2 does a directional char-by-char scan honoring `norm` (true for `[m`/`]M`) to land on the
///     wanted class/method boundary, jumping over other methods via their matching brace.
///
/// Returns the target byte offset, or `cursor` for a no-op / error (Vim beeps and does not move). The
/// result is always an ASCII brace position → a char boundary.
///
/// DIVERGENCE (documented non-goal): braces inside string literals and comments are treated literally.
/// nvim's `findmatch` skips them; ruse has no syntax model, so `]m` over a buffer with a `}` inside a
/// string will stop on that `}`. Verified-identical everywhere else against nvim v0.12.4.
fn method_target(b: &[u8], cursor: usize, forward: bool, end: bool, count: u32) -> usize {
    let old_pos = cursor.min(b.len());
    // findc: '{' for the backward commands, '}' for the forward commands (Vim: cmdchar '[' vs ']').
    let findc_is_open = !forward;
    // norm is TRUE for "[m" and "]M": ((findc == '{') == (nchar == 'm')).
    // nchar == 'm' ⇔ !end. So norm = (findc_is_open == !end).
    let norm = findc_is_open == !end;

    // --- Phase 1: walk out to the outermost enclosing brace. ---
    let mut new_pos: Option<usize> = None;
    let mut prev_pos: Option<usize> = None;
    let mut cur = old_pos;
    // Vim loops 9999 times; nesting depth is bounded by buffer length, so an unbounded walk-out is fine.
    while let Some(pos) = find_enclosing_brace(b, cur, forward) {
        prev_pos = new_pos;
        cur = pos;
        new_pos = Some(pos);
    }

    // --- Phase 2: directional scan for the target boundary. ---
    let mut n = count.max(1) as i64;
    let mut pos: Option<usize>;
    if let Some(pp) = prev_pos {
        // We were inside a method: start from the brace one level inside the class.
        pos = Some(pp);
        cur = pp;
        if norm {
            n -= 1;
        }
    } else {
        pos = None;
        cur = old_pos;
    }

    while n > 0 {
        loop {
            // findc == '{' (backward cmds) scans backward; else forward. Move one char boundary.
            let moved = if findc_is_open {
                if cur == 0 {
                    None
                } else {
                    Some(prev_boundary(b, cur))
                }
            } else {
                let nx = next_boundary(b, cur);
                if nx >= b.len() {
                    None
                } else {
                    Some(nx)
                }
            };
            let Some(nc) = moved else {
                // Ran off the buffer edge.
                if pos.is_none() {
                    return old_pos; // error: nothing found, no move.
                }
                n = 0;
                break;
            };
            cur = nc;
            let c = b[cur];
            if c == b'{' || c == b'}' {
                if (c == if findc_is_open { b'{' } else { b'}' } && norm) || (n == 1 && !norm) {
                    // Found the class boundary we want, or the count-th method boundary.
                    new_pos = Some(cur);
                    pos = Some(cur);
                    n = 0;
                } else if new_pos.is_none() {
                    // Started outside any class and just crossed in — use this boundary.
                    new_pos = Some(cur);
                    pos = Some(cur);
                } else {
                    // Found the start/end of ANOTHER method: jump to its matching brace and go on.
                    match find_enclosing_brace(b, cur, forward) {
                        Some(m) => {
                            pos = Some(m);
                            cur = m;
                        }
                        None => {
                            pos = None;
                            n = 0;
                        }
                    }
                }
                break;
            }
        }
        n -= 1;
    }

    match pos {
        Some(p) => p,
        None => old_pos, // error (Vim beeps): stay put.
    }
}

/// Whether a motion is one of the method (brace-block) family, with `(forward, end)` flags.
fn method_spec(m: Motion) -> Option<(bool, bool)> {
    match m {
        Motion::MethodStartFwd => Some((true, false)),
        Motion::MethodStartBack => Some((false, false)),
        Motion::MethodEndFwd => Some((true, true)),
        Motion::MethodEndBack => Some((false, true)),
        _ => None,
    }
}

/// The (open, close, forward) triple for an unmatched-bracket motion, or `None` for any other motion.
fn unmatched_spec(m: Motion) -> Option<(u8, u8, bool)> {
    match m {
        Motion::UnmatchedParenBack => Some((b'(', b')', false)),
        Motion::UnmatchedParenFwd => Some((b'(', b')', true)),
        Motion::UnmatchedBraceBack => Some((b'{', b'}', false)),
        Motion::UnmatchedBraceFwd => Some((b'{', b'}', true)),
        _ => None,
    }
}

/// Whether motion `m` FAILS from `cursor` (Vim's `clearopbeep`: the command is cancelled, the cursor stays
/// and the typeahead is flushed, #1085). `op` is an operator waiting for the motion; `visual` is a Visual
/// selection, where `l` may still step onto the line break. Checked against nvim v0.12.6:
/// - `h`/`l`/`<BS>`/`<Space>` fail when they cannot move at all (a count moves as far as it can); under an
///   operator `h`/`l` are a silent no-op instead (`dh` at column 0).
/// - `j`/`k`/`+`/`-`/`<CR>` fail only on the last (first) line; a count past the edge stops there.
/// - `w`/`W`/`b`/`B`/`ge`/`gE` fail when they cannot move; `e`/`E` when the last count step cannot. Under an
///   operator `w`/`W` and `e`/`E` reach the buffer end instead (`$dw`, `$de`).
/// - `f`/`t`/`F`/`T`/`;`/`,` without the count-th match, `}`/`{`/`)`/`(` with count left over, `%` without a
///   bracket, `[(`/`])`/`[{`/`]}` without an enclosing one, and `{count}$` (count > 1) on the last line.
/// - `it`/`at` under an operator outside a tag pair (the block, quote and sentence objects fail through
///   `textobj::object_fails`).
///
/// Every other motion (line jumps, `0`, `^`, `$`, `|`, sections, …) never fails here.
///
/// `memo` holds (or receives) the motion's target, `target(b, cursor, m, count)`, so a planner that goes
/// on to move does not run the motion twice (#1231). The line's ends are told by the bytes around a
/// position ([`at_bol`], [`at_nul`]); only the vertical and `{count}$` checks find the line.
#[must_use]
pub(crate) fn fails_with(
    b: &[u8],
    cursor: usize,
    m: Motion,
    count: u32,
    op: bool,
    visual: bool,
    memo: &std::cell::OnceCell<usize>,
) -> bool {
    let n = count.max(1);
    let cur = cursor.min(b.len());
    let last_line = || line_end(b, cur) >= content_end(b);
    // Where the motion leaves an on-char caret: never on a non-empty line's break or the phantom slot.
    let clamp = |t: usize| {
        let t = off_phantom_at(b, t.min(b.len()));
        if at_nul(b, t) && !at_bol(b, t) {
            prev_grapheme(b, t)
        } else {
            t
        }
    };
    let still = || clamp(*memo.get_or_init(|| target(b, cur, m, count))) == clamp(cur);
    match m {
        Motion::Left => !op && at_bol(b, cur),
        Motion::Right if visual => !op && at_nul(b, cur),
        // From the NUL slot the step crosses the `\n`, past the line's end; else it ends on or before it.
        Motion::Right => !op && (at_nul(b, cur) || at_nul(b, next_grapheme(b, cur))),
        Motion::WrapLeft | Motion::WrapRight => !op && still(),
        Motion::Up | Motion::UpFirstNonBlank => line_start(b, cur) == 0,
        Motion::Down | Motion::DownFirstNonBlank => last_line(),
        Motion::WordFwd | Motion::BigWordFwd => !op && still(),
        Motion::WordBack | Motion::BigWordBack | Motion::WordEndBack | Motion::BigWordEndBack => {
            still()
        }
        Motion::WordEnd | Motion::BigWordEnd => {
            !op && {
                let before = if n > 1 { target(b, cur, m, n - 1) } else { cur };
                let t = if n == count {
                    *memo.get_or_init(|| target(b, cur, m, count))
                } else {
                    target(b, cur, m, n)
                };
                clamp(t) == clamp(before)
            }
        }
        Motion::FindChar {
            ch,
            forward,
            till,
            repeat,
        } => find_char_target(b, cur, ch, forward, till, repeat, n).is_none(),
        Motion::ParagraphFwd | Motion::ParagraphBack => {
            paragraph_landing(b, cur, m == Motion::ParagraphFwd, n).is_none()
        }
        Motion::SentenceFwd | Motion::SentenceBack => {
            crate::textobj::sentence_motion_fails(b, cur, m == Motion::SentenceFwd, n)
        }
        Motion::MatchBracket => match_bracket(b, cur).is_none(),
        Motion::InnerParagraph | Motion::AParagraph => {
            paragraph_fails(b, cur, n, m == Motion::AParagraph)
        }
        Motion::UnmatchedParenBack
        | Motion::UnmatchedParenFwd
        | Motion::UnmatchedBraceBack
        | Motion::UnmatchedBraceFwd => still(),
        // `{count}$` moves `count - 1` lines down first, failing like `j` only on the last line (`5$` on
        // the line above it still reaches the last line's end).
        Motion::LineEnd => n > 1 && last_line(),
        // `{count}dd`/`yy`/`cc`/`>>` move `count - 1` lines down first (`nv_lineop` → `cursor_down`), so on
        // the last line a count above 1 fails like `j` there (#1176).
        Motion::Line => op && n > 1 && last_line(),
        // `it`/`at` outside a tag pair (or a count past its nesting) cancel the operator (#1179). The `at`
        // span holds the tags themselves, so it is empty only when there is no pair.
        Motion::Tag { .. } => {
            op && {
                let (s, e) = tag_span(b, cur, true, n);
                s == e
            }
        }
        _ => false,
    }
}

/// The cursor target for a bare move (`w`, `3l`, `k`, …), applying `count`.
#[must_use]
pub(crate) fn target(b: &[u8], cursor: usize, m: Motion, count: u32) -> usize {
    let n = count.max(1);
    let cur0 = cursor.min(b.len());
    // Char-search resolves the count-th match directly (repeating a single `t` step would stick in place).
    if let Motion::FindChar {
        ch,
        forward,
        till,
        repeat,
    } = m
    {
        return find_char_target(b, cur0, ch, forward, till, repeat, count).unwrap_or(cur0);
    }
    // Line jumps use the count as an absolute line number (or the last line), not a repeat count.
    if m == Motion::GotoLine {
        return first_non_blank(b, nth_line_start(b, count));
    }
    if m == Motion::LastLine {
        return first_non_blank(b, last_line_start(b));
    }
    // `{count}go`: the count is an absolute 1-based BYTE offset (not a repeat); a count past EOF lands on the
    // LAST byte (Vim never rests the cursor past the final char). Snapped to a char boundary.
    // A terminated buffer's last byte is its final newline, so a count past EOF lands on the last line's last
    // char instead (#677).
    if m == Motion::GotoByte {
        let t = off_phantom_at(b, (n as usize).saturating_sub(1).min(b.len()));
        return snap(b, t.min(b.len().saturating_sub(1)));
    }
    // Bracket match jumps to a single computed position (count is not a repeat; `count%` is deferred).
    if m == Motion::MatchBracket {
        return match_bracket(b, cur0).unwrap_or(cur0);
    }
    // `{count}%`: the count is a PERCENTAGE — go to line `(count*line_count + 99)/100` (Vim), first
    // non-blank. `line_count` follows the terminator model (a trailing `\n` is not a new line).
    if m == Motion::GotoPercent {
        let nl = crate::pos::count_newlines(b);
        let line_count = if b.is_empty() {
            1
        } else if b.last() == Some(&b'\n') {
            nl
        } else {
            nl + 1
        };
        let line = ((n as usize) * line_count)
            .div_ceil(100)
            .clamp(1, line_count) as u32;
        return first_non_blank(b, nth_line_start(b, line));
    }
    // `{count}g_`: the count is `count-1` lines down, then the last non-blank char (not a repeat).
    // Stops at the last line, never the phantom slot after a final newline (#677).
    if m == Motion::LineLastNonBlank {
        return last_non_blank(b, lines_down(b, cur0, n - 1));
    }
    // `+`/`<CR>` (down), `-` (up), `_` (count-1 down): the count is a line delta (not a repeat), then the
    // first non-blank char of the target line. Clamped at the buffer edges.
    if matches!(
        m,
        Motion::DownFirstNonBlank | Motion::UpFirstNonBlank | Motion::LineUnderscore
    ) {
        let delta: isize = match m {
            Motion::DownFirstNonBlank => n as isize,
            Motion::LineUnderscore => (n - 1) as isize,
            _ => -(n as isize), // UpFirstNonBlank
        };
        let mut ls = line_start(b, cur0);
        if delta >= 0 {
            for _ in 0..delta {
                let le = line_end(b, ls);
                if le >= content_end(b) {
                    break; // clamp at the last content line, not the trailing phantom
                }
                ls = le + 1;
            }
        } else {
            for _ in 0..(-delta) {
                if ls == 0 {
                    break;
                }
                ls = line_start(b, ls - 1);
            }
        }
        return first_non_blank(b, ls);
    }
    // Section motions (`]]`/`[[`/`][`/`[]`): jump `count` section boundaries and rest on the target line's
    // first non-blank (column 0 for a brace/form-feed line). The count is a repeat, resolved by the scanner.
    if matches!(
        m,
        Motion::SectionFwd | Motion::SectionBack | Motion::SectionEndFwd | Motion::SectionEndBack
    ) {
        let ls = match m {
            Motion::SectionFwd => next_section_line(b, cur0, n, false),
            Motion::SectionEndFwd => next_section_line(b, cur0, n, true),
            Motion::SectionBack => prev_section_line(b, cur0, n, false),
            _ => prev_section_line(b, cur0, n, true), // SectionEndBack
        };
        return first_non_blank(b, ls);
    }
    // Unmatched-bracket motions (`[(`/`])`/`[{`/`]}`): the count is a repeat (levels to step out); the scanner
    // resolves it directly to a single target (or `cur0` when there is no enclosing unmatched bracket).
    if let Some((open, close, forward)) = unmatched_spec(m) {
        return unmatched_bracket(b, cur0, open, close, forward, n);
    }
    // Method (brace-block) motions (`]m`/`[m`/`]M`/`[M`): the count is baked into the two-phase scan
    // (Vim `nv_bracket_block`), which resolves directly to a single target (or `cur0` for a no-op).
    if let Some((forward, end)) = method_spec(m) {
        return method_target(b, cur0, forward, end, n);
    }
    // `{count}|`: the count is the 1-based display column of the current line (not a repeat).
    if m == Motion::Column {
        return at_col(b, line_start(b, cur0), (n - 1) as usize);
    }
    // `gM` / `{count}gM`: the count is a PERCENTAGE of the line's character length (not a repeat); land on
    // char index `min(count*chars/100, chars-1)`. The frontend passes 50 for a bare `gM`.
    if m == Motion::MidLine {
        let ls = line_start(b, cur0);
        let chars = col_of(b, ls, line_end(b, cur0)); // char count of the line (excludes the newline)
        if chars == 0 {
            return ls; // empty line — nowhere to go
        }
        let idx = ((n as usize) * chars / 100).min(chars - 1);
        return at_col(b, ls, idx);
    }
    // `{count}$` / `{count}<End>`: the end of the line `count - 1` lines down (stopping at the last line).
    if m == Motion::LineEnd && n > 1 {
        return target(b, lines_down(b, cur0, n - 1), m, 1);
    }
    // `)` / `(`: Vim's `findsent`, ported in `textobj` for `is`/`as`, where an empty line is a sentence of
    // its own (`)` from `foo bar` stops on the blank line below it).
    if matches!(m, Motion::SentenceFwd | Motion::SentenceBack) {
        return crate::textobj::sentence_motion(b, cur0, m == Motion::SentenceFwd, n);
    }
    // `}` / `{`: a count that runs out of paragraphs fails as a whole (see `paragraph_target`).
    if matches!(m, Motion::ParagraphFwd | Motion::ParagraphBack) {
        return paragraph_target(b, cur0, m == Motion::ParagraphFwd, n);
    }
    let mut c = cur0;
    // `{count}h` / `{count}l` step through one line cache: a count is not a whole-line scan per step (#1224).
    let steps = Steps::new(b);
    for _ in 0..n {
        let prev = c;
        c = match m {
            // `h`/`l` are bounded by the line; the count stops at its edge.
            // A step is one grapheme cluster (F-002, #696): a composed char or a ZWJ emoji is one char.
            Motion::Left if c > steps.line_start(c) => steps.prev(c),
            Motion::Right if c < steps.line_end(c) => steps.next(c),
            Motion::Left | Motion::Right => c,
            Motion::WrapLeft => wrap_left(b, c),
            Motion::WrapRight => wrap_right(b, c),
            Motion::EmacsCharBack => prev_boundary(b, c),
            Motion::EmacsCharFwd => next_boundary(b, c),
            Motion::Up => up(b, c),
            Motion::Down => down(b, c),
            Motion::LineStart => line_start(b, c),
            // Vim `^` / Emacs `back-to-indentation` (M-m): the first non-blank of the line (or the line end
            // when the line is all blank). Count-agnostic — repeating stays on the same line.
            Motion::LineFirstNonBlank => first_non_blank(b, line_start(b, c)),
            // A bare `$` lands ON the last char of the line (Vim never rests the cursor past it in Normal),
            // unlike the `d$` operator span which reaches the line end — see `char_span`. `{count}$` is the
            // end of the line `count - 1` below (resolved by the early return above).
            Motion::LineEnd => {
                let ls = line_start(b, c);
                let le = line_end(b, c);
                if le > ls {
                    prev_grapheme(b, le)
                } else {
                    le
                }
            }
            Motion::WordFwd => next_word_start(&steps, c, false),
            Motion::WordBack => prev_word_start(&steps, c, false),
            Motion::WordEnd => steps.prev(word_end_in(&steps, c, false)), // land ON the last char
            Motion::WordEndBack => prev_word_end(&steps, c, false),       // Vim `ge`
            Motion::EmacsWordFwd => emacs_word_fwd(b, c), // Emacs forward-word: two-class, land AFTER the word
            Motion::EmacsWordBack => emacs_word_back(b, c), // Emacs backward-word: two-class mirror
            Motion::BigWordFwd => next_word_start(&steps, c, true),
            Motion::BigWordBack => prev_word_start(&steps, c, true),
            Motion::BigWordEnd => steps.prev(word_end_in(&steps, c, true)),
            Motion::BigWordEndBack => prev_word_end(&steps, c, true), // Vim `gE`
            // FindChar / line-jumps are resolved by the early returns above; objects/linewise have no
            // bare-move target.
            Motion::FindChar { .. }
            | Motion::GotoLine
            | Motion::LastLine
            | Motion::DownFirstNonBlank
            | Motion::UpFirstNonBlank
            | Motion::LineUnderscore
            | Motion::GotoByte
            | Motion::MatchBracket
            | Motion::GotoPercent
            | Motion::LineLastNonBlank
            | Motion::Column
            | Motion::MidLine
            | Motion::InnerWord
            | Motion::AWord
            | Motion::InnerBigWord
            | Motion::ABigWord
            | Motion::InnerParagraph
            | Motion::AParagraph
            | Motion::InnerSentence
            | Motion::ASentence
            | Motion::SectionFwd
            | Motion::SectionBack
            | Motion::SectionEndFwd
            | Motion::SectionEndBack
            | Motion::UnmatchedParenBack
            | Motion::UnmatchedParenFwd
            | Motion::UnmatchedBraceBack
            | Motion::UnmatchedBraceFwd
            | Motion::MethodStartFwd
            | Motion::MethodStartBack
            | Motion::MethodEndFwd
            | Motion::MethodEndBack
            | Motion::Pair { .. }
            | Motion::Quote { .. }
            | Motion::Tag { .. }
            | Motion::ParagraphFwd
            | Motion::ParagraphBack
            | Motion::SentenceFwd
            | Motion::SentenceBack
            | Motion::Line => c,
        };
        // Every step is a pure function of `(b, c)`, so a step that did not move never will: stop. Without
        // this a hostile count (`emacs_kill_word 4294967295` in a trace, or a huge `C-u` prefix) spun for
        // minutes at the buffer edge (#907).
        if c == prev {
            break;
        }
    }
    c
}

/// The start of the line `n` lines below `pos`'s line, stopping at the last line. A trailing newline
/// does not open a further line (the terminator model).
fn lines_down(b: &[u8], pos: usize, n: u32) -> usize {
    let mut ls = line_start(b, pos);
    for _ in 0..n {
        let le = line_end(b, ls);
        if le + 1 >= b.len() {
            break;
        }
        ls = le + 1;
    }
    ls
}

/// One bare `<Space>` step (`'whichwrap'` `s`): the next char on the line, else the first char of the next
/// line (an empty line is a stop of its own). The buffer's last char stays put.
fn wrap_right(b: &[u8], c: usize) -> usize {
    let le = line_end(b, c);
    let n = next_boundary(b, c);
    if n < le {
        n
    } else if le + 1 < b.len() {
        le + 1
    } else {
        c
    }
}

/// One bare `<BS>` step (`'whichwrap'` `b`): the previous char on the line, else the last char of the
/// previous line (its newline when that line is empty). The buffer's first char stays put.
fn wrap_left(b: &[u8], c: usize) -> usize {
    let ls = line_start(b, c);
    if c > ls {
        return prev_boundary(b, c);
    }
    if ls == 0 {
        return c;
    }
    let prev_nl = ls - 1;
    if prev_nl > line_start(b, prev_nl) {
        prev_boundary(b, prev_nl)
    } else {
        prev_nl
    }
}

/// Apply the pure step `f` up to `n` times from `start`, stopping early at a fixpoint: a step that did not
/// move never will, so a hostile count costs at most one step per byte instead of `n` steps (#907).
fn repeat_step(n: u32, start: usize, f: impl Fn(usize) -> usize) -> usize {
    let mut c = start;
    for _ in 0..n {
        let next = f(c);
        if next == c {
            break;
        }
        c = next;
    }
    c
}

/// The charwise byte range `[start, end)` an operator (`d`/`c`) covers for a motion + count. Backward
/// motions produce a range ending at the cursor; `WordEnd` is inclusive (Vim `de`).
///
/// A Vim span never takes a terminated buffer's final newline (#677): Vim cannot delete the last line's
/// end, so a motion running off the end (`d)`, `c}`, `de` past the last word) stops at the last line's end,
/// which covers its last char. The linewise paragraph objects (whole lines) and the Emacs motions (whose
/// point may sit after the final newline, so `kill-word` there can take it) are exempt.
#[must_use]
pub(crate) fn char_span(b: &[u8], cursor: usize, m: Motion, count: u32) -> (usize, usize) {
    let (s, e) = char_span_raw(b, cursor, m, count);
    let exempt = matches!(
        m,
        Motion::InnerParagraph
            | Motion::AParagraph
            | Motion::EmacsCharFwd
            | Motion::EmacsCharBack
            | Motion::EmacsWordFwd
            | Motion::EmacsWordBack
    );
    if exempt {
        (s, e)
    } else {
        let ce = content_end(b);
        (s.min(ce), e.min(ce))
    }
}

fn char_span_raw(b: &[u8], cursor: usize, m: Motion, count: u32) -> (usize, usize) {
    let cur = cursor.min(b.len());
    let n = count.max(1);
    match m {
        // forward / rightward → [cursor, target). `d<Space>` stays on the line exactly like `dl` (nvim).
        Motion::Right | Motion::WrapRight => (cur, target(b, cur, Motion::Right, n)),
        Motion::EmacsCharFwd => (cur, target(b, cur, m, n)),
        // `d<BS>` counts the newline as one step, so at column 0 it joins onto the line above (nvim).
        Motion::WrapLeft => (repeat_step(n, cur, |c| prev_boundary(b, c)), cur),
        // `dw`/`dW`: the LAST word moved over does not cross its line's newline — the operated text ends at
        // the end of that line, through any trailing white space — so `dw` on the last word EMPTIES the line
        // instead of joining it to the next (nvim: `dw` on ["foo   ","bar"] → ["","bar"]). Earlier counts
        // cross line breaks (`3dw`, #742), and an empty line is a word of its own (#746): `dw` there ends at
        // the next line's start, which the exclusive-linewise rule in `op_span` turns into a linewise delete.
        Motion::WordFwd | Motion::BigWordFwd => (
            cur,
            word_fwd(&Steps::new(b), cur, m == Motion::BigWordFwd, n, true),
        ),
        // `d$` reaches the line end (deletes the last char too), unlike the bare `$` move which stops on it.
        // `d{count}$` reaches the end of the line `count - 1` below.
        Motion::LineEnd => (cur, line_end(b, lines_down(b, cur, n - 1))),
        // Paragraph motions as a charwise span (`c}` / a Visual `}` extend): forward → [cursor, target),
        // backward → [target, cursor). The operator's exclusive-linewise reshaping lives in `editor::op_span`.
        // At the end of the buffer `}` lands ON the last char and is inclusive (Vim), so the span takes it; a
        // blank-line landing is a line start (its `\n`), the exclusive end.
        Motion::ParagraphFwd => match paragraph_landing(b, cur, true, n) {
            None => (cur, cur),
            Some((t, true)) if t < b.len() && b[t] != b'\n' => (cur, next_grapheme(b, t)),
            Some((t, _)) => (cur, t),
        },
        Motion::ParagraphBack => (target(b, cur, m, n), cur),
        // Sentence motions are exclusive charwise: forward → [cursor, next), backward → [prev, cursor).
        Motion::SentenceFwd => (cur, target(b, cur, m, n)),
        Motion::SentenceBack => (target(b, cur, m, n), cur),
        // Section motions as a charwise span (`c]]` / a Visual `]]` extend): forward → [cursor, target),
        // backward → [target, cursor). The operator's exclusive-linewise reshaping lives in `editor::op_span`
        // (shared with the paragraph motions), so `d]]`/`d[[` come out linewise exactly like `d}`/`d{`.
        Motion::SectionFwd | Motion::SectionEndFwd => (cur, target(b, cur, m, n)),
        Motion::SectionBack | Motion::SectionEndBack => (target(b, cur, m, n), cur),
        // Unmatched-bracket motions are EXCLUSIVE charwise (Vim): forward (`])`/`]}`) → [cursor, target),
        // backward (`[(`/`[{`) → [target, cursor). The exclusive-linewise reduction that can make them
        // whole-line (a target at column 0) lives in `editor::op_span`, shared with the paragraph/section
        // motions. A no-op target (== cursor) collapses to an empty range there.
        Motion::UnmatchedParenFwd | Motion::UnmatchedBraceFwd => (cur, target(b, cur, m, n)),
        Motion::UnmatchedParenBack | Motion::UnmatchedBraceBack => (target(b, cur, m, n), cur),
        // Method motions are EXCLUSIVE charwise like the unmatched-bracket ones: forward (`]m`/`]M`) →
        // [cursor, target), backward (`[m`/`[M`) → [target, cursor). The exclusive-linewise reduction
        // (a target at column 0 → whole lines) lives in `editor::op_span`. No-op target collapses there.
        Motion::MethodStartFwd | Motion::MethodEndFwd => (cur, target(b, cur, m, n)),
        Motion::MethodStartBack | Motion::MethodEndBack => (target(b, cur, m, n), cur),
        // inclusive end-of-word (Vim `de` / `dE`).
        Motion::WordEnd | Motion::BigWordEnd => {
            // Each step starts from the previous landing (the last char of a word), not its exclusive end, so
            // `2de` on `foo.bar` takes `foo.` (the `.` is a word of its own).
            let big = m == Motion::BigWordEnd;
            let s = Steps::new(b);
            let land = repeat_step(n, cur, |p| s.prev(word_end_in(&s, p, big)));
            (cur, next_grapheme(b, land).max(cur))
        }
        // Emacs `kill-word` (`M-d`): `[cursor, emacs_word_fwd)` — the two-class forward-word span, which
        // (unlike Vim `de`) skips a leading non-word run and stops at `_`/punctuation boundaries.
        Motion::EmacsWordFwd => (cur, repeat_step(n, cur, |e| emacs_word_fwd(b, e))),
        // backward / leftward → [target, cursor)
        Motion::Left
        | Motion::EmacsCharBack
        | Motion::WordBack
        | Motion::EmacsWordBack
        | Motion::BigWordBack
        | Motion::LineStart => (target(b, cur, m, n), cur),
        // `ge`/`gE` are INCLUSIVE backward: span from the previous word-end (landed-on char included) up to
        // and including the cursor char (Vim `dge` on `r` of "foo bar" → "fo").
        Motion::WordEndBack | Motion::BigWordEndBack => {
            let big = m == Motion::BigWordEndBack;
            let s = Steps::new(b);
            let e = repeat_step(n, cur, |e| prev_word_end(&s, e, big));
            (e, next_grapheme(b, cur))
        }
        // `^` / `|` / `go` / `gM` are exclusive and can point either way (indent / column / byte / line-%,
        // left or right): ordered pair from the cursor to the absolute target.
        Motion::LineFirstNonBlank | Motion::Column | Motion::GotoByte | Motion::MidLine => {
            let t = target(b, cur, m, n);
            (cur.min(t), cur.max(t))
        }
        // `g_` is INCLUSIVE (Vim `dg_` deletes through the last non-blank char).
        Motion::LineLastNonBlank => {
            let t = target(b, cur, m, n);
            (cur.min(t), next_grapheme(b, cur.max(t)))
        }
        // char-search: forward includes through the landing char (`dfx` incl. x, `dtx` up to x); backward
        // spans from the landing to the cursor. A missing match is a no-op range.
        Motion::FindChar {
            ch,
            forward,
            till,
            repeat,
        } => match find_char_target(b, cur, ch, forward, till, repeat, n) {
            None => (cur, cur),
            Some(t) if forward => (cur, next_grapheme(b, t)),
            Some(t) => (t, cur),
        },
        // bracket match: inclusive of both brackets and everything between (Vim `d%`). No match → no-op.
        Motion::MatchBracket => match match_bracket(b, cur) {
            Some(m) if m != cur => {
                let (lo, hi) = (cur.min(m), cur.max(m));
                (lo, next_grapheme(b, hi))
            }
            _ => (cur, cur),
        },
        // Word, sentence, bracket and quote objects: Vim's operator-pending range (see `textobj`), before
        // the exclusive-linewise reduction an operator applies (`editor::op_span`).
        Motion::InnerWord
        | Motion::AWord
        | Motion::InnerBigWord
        | Motion::ABigWord
        | Motion::InnerSentence
        | Motion::ASentence
        | Motion::Pair { .. }
        | Motion::Quote { .. } => {
            let (s, e, _) = crate::textobj::op_range(b, cur, m, n);
            (s, e)
        }
        Motion::InnerParagraph => paragraph_span(b, cur, n, false),
        Motion::AParagraph => paragraph_span(b, cur, n, true),
        Motion::Tag { around } => tag_span(b, cur, around, n),
        // vertical / linewise motions are not charwise — callers handle Line / line-jumps specially
        Motion::Up
        | Motion::Down
        | Motion::Line
        | Motion::GotoLine
        | Motion::LastLine
        | Motion::DownFirstNonBlank
        | Motion::UpFirstNonBlank
        | Motion::LineUnderscore
        | Motion::GotoPercent => (cur, cur),
    }
}

#[cfg(test)]
mod word_class_tests {
    //! Word motions split where Vim's character class changes and step by grapheme cluster (#1136); the
    //! byte offsets are nvim 0.12.6's (`wcls_*` oracle fixtures).
    use super::{target, Motion};

    #[test]
    fn small_words_split_on_class_changes() {
        let b = "x API를 y".as_bytes();
        assert_eq!(target(b, 2, Motion::WordFwd, 1), 5); // "API" | "를"
        assert_eq!(target(b, 2, Motion::WordEnd, 1), 4);
        assert_eq!(target(b, 9, Motion::WordBack, 1), 5);
        assert_eq!(target(b, 9, Motion::WordEndBack, 1), 5);
        assert_eq!(target(b, 2, Motion::BigWordFwd, 1), 9);
        let s = "abc한글か漢字カナ x".as_bytes();
        assert_eq!(target(s, 0, Motion::WordFwd, 4), 18); // past abc, 한글, か, 漢字
        assert_eq!(target("😀😀a b".as_bytes(), 0, Motion::WordFwd, 1), 8);
        assert_eq!(target("a\u{a0}b c".as_bytes(), 0, Motion::WordFwd, 1), 3); // NBSP is blank
        assert_eq!(target("a×b c".as_bytes(), 0, Motion::WordFwd, 1), 5); // Latin-1 × is a word char
    }

    #[test]
    fn a_cluster_takes_its_first_chars_class() {
        // 가 + U+0301 is one cluster of the Hangul class, so `w` does not stop on the mark.
        assert_eq!(target("가\u{301}b c".as_bytes(), 0, Motion::WordFwd, 1), 5);
        assert_eq!(target("x 가\u{301}b".as_bytes(), 7, Motion::WordBack, 1), 2);
        // `.` + U+0301 is one punctuation cluster.
        assert_eq!(target("a.\u{301}b c".as_bytes(), 0, Motion::WordFwd, 2), 4);
    }
}

#[cfg(test)]
mod emacs_word_tests {
    //! Emacs `forward-word` / `kill-word` (`M-f` / `M-d`) — the between-character word-forward motion.
    use super::{char_span, target, Motion};

    #[test]
    fn emacs_word_fwd_lands_after_the_word() {
        let b = b"foo bar baz";
        // forward-word from the start rests point AFTER "foo" (index 3), where Vim `e` rests ON `o` (index 2).
        assert_eq!(target(b, 0, Motion::EmacsWordFwd, 1), 3);
        assert_eq!(target(b, 0, Motion::WordEnd, 1), 2);
        // From a space it skips to the end of the next word.
        assert_eq!(target(b, 3, Motion::EmacsWordFwd, 1), 7);
        // Mid-word it stops at the end of the current word.
        assert_eq!(target(b, 1, Motion::EmacsWordFwd, 1), 3);
        // Counted: two words forward.
        assert_eq!(target(b, 0, Motion::EmacsWordFwd, 2), 7);
    }

    #[test]
    fn emacs_kill_word_span_is_the_word_only() {
        // `kill-word` deletes `[cursor, word_end_excl)` — "foo", not Vim `dw`'s "foo " (which eats the space).
        assert_eq!(
            char_span(b"foo bar baz", 0, Motion::EmacsWordFwd, 1),
            (0, 3)
        );
        // From mid-buffer it takes the rest of the current word: "foobar baz" at index 3 kills "bar".
        assert_eq!(char_span(b"foobar baz", 3, Motion::EmacsWordFwd, 1), (3, 6));
    }

    #[test]
    fn emacs_word_fwd_is_two_class_over_punct_and_underscore() {
        // Oracle-pinned (GNU Emacs 30.2): Emacs `forward-word` uses a TWO-class syntax split. A leading
        // punctuation run is SKIPPED (not its own word as in Vim `e`): "...foo" reaches the end of "foo".
        assert_eq!(target(b"...foo", 0, Motion::EmacsWordFwd, 1), 6);
        // `_` is a NON-word char (symbol syntax) in fundamental-mode, so `foo_bar` is two words and
        // forward-word / kill-word stop after "foo" — where Vim would treat `_` as a word char and take all.
        assert_eq!(target(b"foo_bar", 0, Motion::EmacsWordFwd, 1), 3);
        assert_eq!(char_span(b"foo_bar", 0, Motion::EmacsWordFwd, 1), (0, 3));
        // A leading whitespace run is skipped too (kill-word from a space eats through the next word).
        assert_eq!(char_span(b"  foo", 0, Motion::EmacsWordFwd, 1), (0, 5));
    }

    #[test]
    fn emacs_word_back_is_the_two_class_mirror() {
        // Oracle-pinned: `backward-word` skips a trailing non-word run then moves over the word to its start.
        // From inside "bar" of "foo.bar" it skips the "." and lands at the start of "foo" (index 0) — where
        // Vim `b` (WordBack) stops at the "." (index 3, punctuation being its own word).
        assert_eq!(target(b"foo.bar", 4, Motion::EmacsWordBack, 1), 0);
        assert_eq!(target(b"foo.bar", 4, Motion::WordBack, 1), 3);
        // From the end of "foo.bar" it lands at the start of "bar" (skips nothing, over "bar").
        assert_eq!(target(b"foo.bar", 7, Motion::EmacsWordBack, 1), 4);
        // `_` is non-word: from the end of "foo_bar" backward-word stops at the start of "bar" (index 4).
        assert_eq!(target(b"foo_bar", 7, Motion::EmacsWordBack, 1), 4);
        // Backward-kill-word span crossing the "." run: [emacs_word_back, cursor) = [0, 4) = "foo.".
        assert_eq!(char_span(b"foo.bar", 4, Motion::EmacsWordBack, 1), (0, 4));
        // At buffer start it is inert.
        assert_eq!(target(b"foo bar", 0, Motion::EmacsWordBack, 1), 0);
    }
}

#[cfg(test)]
mod backward_word_end_tests {
    //! Vim `ge` / `gE` — backward to the end of the previous word / WORD.
    use super::{char_span, target, Motion};

    #[test]
    fn ge_lands_on_previous_word_end() {
        let b = b"foo bar"; // f0 o1 o2 sp3 b4 a5 r6
                            // From the last char, `ge` lands ON the previous word's last char ('o' of foo, index 2).
        assert_eq!(target(b, 6, Motion::WordEndBack, 1), 2);
        // From a word start ('b', index 4) → still the previous word's end.
        assert_eq!(target(b, 4, Motion::WordEndBack, 1), 2);
        // From within/at the first word with no earlier word → clamps to 0.
        assert_eq!(target(b, 2, Motion::WordEndBack, 1), 0);
        // Counted: two word-ends back from a later buffer.
        assert_eq!(target(b"aa bb cc", 7, Motion::WordEndBack, 2), 1); // cc→bb-end→aa-end(idx1)
    }

    #[test]
    fn big_ge_treats_punctuation_as_part_of_the_word() {
        let b = b"foo.bar baz"; // small `ge` stops at the punct boundary; big `gE` does not
                                // `ge` from 'baz' start (index 8) → end of "bar" (index 6) since punct/word split into groups.
        assert_eq!(target(b, 8, Motion::WordEndBack, 1), 6);
        // `gE` from index 8 → end of the WORD "foo.bar" which is also index 6 (its last char 'r').
        assert_eq!(target(b, 8, Motion::BigWordEndBack, 1), 6);
        // But inside "foo.bar": `ge` from 'b'(index4) → end of "foo" segment... the '.' at 3 is its own
        // small-word (punct), so `ge` lands on it (index 3); `gE` skips to "foo" end? No earlier WORD, → 0.
        assert_eq!(target(b, 4, Motion::WordEndBack, 1), 3);
        assert_eq!(target(b, 4, Motion::BigWordEndBack, 1), 0);
    }

    #[test]
    fn dge_is_inclusive_of_both_ends() {
        // `dge` on 'r' of "foo bar" deletes back through the previous word-end, inclusive: [2, 7) → "fo".
        assert_eq!(char_span(b"foo bar", 6, Motion::WordEndBack, 1), (2, 7));
    }
}

#[cfg(test)]
mod vim_line_model_tests {
    //! The v0.1.3 motion cluster at the `target`/`char_span` level: empty lines as words (#746), counted
    //! `dw` (#742), multibyte `e` (#741), `}` from a blank line (#748), `;` after `t` (#802).
    use super::{char_span, target, Motion};

    fn till(forward: bool, repeat: bool) -> Motion {
        Motion::FindChar {
            ch: 'x',
            forward,
            till: true,
            repeat,
        }
    }

    #[test]
    fn word_motions_stop_on_empty_lines() {
        let b = b"foo\n\nbar"; // empty line at 4
        assert_eq!(target(b, 0, Motion::WordFwd, 1), 4);
        assert_eq!(target(b, 0, Motion::WordFwd, 2), 5);
        assert_eq!(target(b, 5, Motion::WordBack, 1), 4);
        assert_eq!(target(b, 5, Motion::BigWordBack, 1), 4);
        assert_eq!(target(b, 5, Motion::WordEndBack, 1), 4);
        // `e` does not stop there (Vim `end_word` skips empty lines).
        assert_eq!(target(b, 2, Motion::WordEnd, 1), 7);
        // `dw` on the empty line reaches the next line's start (made linewise by the operator).
        assert_eq!(char_span(b, 4, Motion::WordFwd, 1), (4, 5));
    }

    #[test]
    fn counted_dw_crosses_lines_until_its_last_word() {
        let b = b"foo bar\nbaz qux";
        assert_eq!(char_span(b, 0, Motion::WordFwd, 3), (0, 12)); // through "baz "
        assert_eq!(char_span(b, 0, Motion::WordFwd, 2), (0, 7)); // last word "bar" ends its line
        assert_eq!(char_span(b, 4, Motion::WordFwd, 1), (4, 7));
    }

    #[test]
    fn e_advances_over_multibyte_words() {
        let b = "가나 다라".as_bytes(); // 가0 나3 sp6 다7 라10
        assert_eq!(target(b, 0, Motion::WordEnd, 1), 3);
        assert_eq!(target(b, 3, Motion::WordEnd, 1), 10);
        assert_eq!(char_span(b, 3, Motion::WordEnd, 1), (3, 13));
    }

    #[test]
    fn paragraph_from_a_blank_line_skips_the_blank_run() {
        let b = b"a\n\n\nb\n\nc"; // blanks at 2, 3 and 6
        assert_eq!(target(b, 2, Motion::ParagraphFwd, 1), 6);
        assert_eq!(target(b, 6, Motion::ParagraphBack, 1), 3);
        // A count that runs out of paragraphs fails; the last step alone lands on the last char.
        assert_eq!(target(b"a\nbc\n", 0, Motion::ParagraphFwd, 2), 0);
        assert_eq!(target(b"a\nbc\n", 0, Motion::ParagraphFwd, 1), 3);
        assert_eq!(char_span(b"a\nbc\n", 0, Motion::ParagraphFwd, 1), (0, 4));
    }

    #[test]
    fn repeated_till_skips_the_adjacent_match() {
        let b = b"axbxcxd";
        assert_eq!(
            target(b, 0, till(true, false), 1),
            0,
            "`tx` stops before the x at 1"
        );
        assert_eq!(
            target(b, 0, till(true, true), 1),
            2,
            "`;` goes on to before the next x"
        );
        assert_eq!(
            target(b, 2, till(true, true), 2),
            4,
            "`2;` counts the adjacent match"
        );
        assert_eq!(target(b, 6, till(false, true), 1), 4, "`;` after `Tx`");
    }

    #[test]
    fn off_phantom_moves_only_the_slot_after_a_final_newline() {
        assert_eq!(super::off_phantom_at(b"ab\n", 3), 1);
        assert_eq!(super::off_phantom_at(b"a\n\n", 3), 2);
        assert_eq!(
            super::off_phantom_at(b"ab", 2),
            2,
            "an unterminated end is the last line's end"
        );
        assert_eq!(super::off_phantom_at(b"", 0), 0);
        assert_eq!(
            super::off_phantom_at("a가\n".as_bytes(), 5),
            1,
            "lands on a char boundary"
        );
    }
}

#[cfg(test)]
mod gunderscore_column_tests {
    //! Vim `g_` (last non-blank) and `|` (go to column).
    use super::{char_span, target, Motion};

    #[test]
    fn g_underscore_lands_on_last_non_blank() {
        let b = b"  foo bar  \nx\n"; // line 0: 'r' is the last non-blank at index 8
        assert_eq!(target(b, 0, Motion::LineLastNonBlank, 1), 8);
        assert_eq!(
            target(b, 8, Motion::LineLastNonBlank, 1),
            8,
            "idempotent on the char"
        );
        // `dg_` is inclusive: from 'f' (index 2) through 'r' → "foo bar".
        assert_eq!(char_span(b, 2, Motion::LineLastNonBlank, 1), (2, 9));
    }

    #[test]
    fn pipe_goes_to_the_count_th_column() {
        let b = b"abcdef\n";
        assert_eq!(target(b, 0, Motion::Column, 1), 0, "1| = column 1");
        assert_eq!(target(b, 0, Motion::Column, 5), 4, "5| = column 5");
        // Past the line end clamps to the last column.
        assert_eq!(target(b, 0, Motion::Column, 99), 6);
        // `d5|` from column 1 spans the ordered pair [0, 4).
        assert_eq!(char_span(b, 0, Motion::Column, 5), (0, 4));
    }

    #[test]
    fn gm_lands_at_the_percent_char_of_the_line() {
        // Landings VERIFIED against nvim v0.12.4 (`gM` = 50% by char count; 0-based index = 50*L/100).
        // "abcdefghij" (L=10): bare gM (count 50) → index 5 ('f').
        let b = b"abcdefghij\n";
        assert_eq!(target(b, 0, Motion::MidLine, 50), 5, "gM = middle char");
        assert_eq!(target(b, 9, Motion::MidLine, 50), 5, "gM ignores start col");
        // `{count}gM` as a percentage (nvim: 25gM→col3/idx2, 100gM→last/idx9, 10gM→idx1, 1gM→idx0).
        assert_eq!(target(b, 0, Motion::MidLine, 25), 2, "25gM");
        assert_eq!(
            target(b, 0, Motion::MidLine, 100),
            9,
            "100gM clamps to last char"
        );
        assert_eq!(target(b, 0, Motion::MidLine, 10), 1, "10gM");
        assert_eq!(target(b, 0, Motion::MidLine, 1), 0, "1gM");
        // Odd length floors: "abcde" (L=5) → 50*5/100 = 2 (nvim col 3).
        let odd = b"abcde\n";
        assert_eq!(
            target(odd, 0, Motion::MidLine, 50),
            2,
            "gM on a 5-char line"
        );
        // Single char / empty line: stay put at the line start.
        assert_eq!(target(b"a\n", 0, Motion::MidLine, 50), 0);
        assert_eq!(target(b"\n", 0, Motion::MidLine, 50), 0, "empty line");
        // `dgM` is exclusive charwise (nvim: `0dgM` on "abcdefghij" → "fghij" = span [0,5); `$dgM` → [5,9)).
        assert_eq!(char_span(b, 0, Motion::MidLine, 50), (0, 5));
        assert_eq!(char_span(b, 9, Motion::MidLine, 50), (5, 9));
        // gM counts leading whitespace too: "    abcdef" (L=10) → idx 5 (nvim col 6).
        assert_eq!(target(b"    abcdef\n", 0, Motion::MidLine, 50), 5);
    }
}

#[cfg(test)]
mod line_first_non_blank_motion_tests {
    //! Vim `+` / `<CR>` (down), `-` (up), `_` (count-1 down) — line motions landing on the first
    //! non-blank of the target line. (Operator linewise-ness is covered in `editor::tests`.)
    use super::{target, Motion};

    // "a\n  bc\n   d\nef\n": line starts 0,2,7,12; first-non-blanks 0,4,10,12.
    const B: &[u8] = b"a\n  bc\n   d\nef\n";

    #[test]
    fn plus_goes_count_lines_down_to_first_non_blank() {
        assert_eq!(
            target(B, 0, Motion::DownFirstNonBlank, 1),
            4,
            "+ -> line 1 'b'"
        );
        assert_eq!(
            target(B, 0, Motion::DownFirstNonBlank, 2),
            10,
            "2+ -> line 2 'd'"
        );
        // Past the last line clamps to the last CONTENT line ('ef' at byte 12). The trailing '\n' is a
        // terminator, not a navigable empty line (Vim), so `+` never lands on the phantom slot at byte 15.
        assert_eq!(
            target(B, 0, Motion::DownFirstNonBlank, 99),
            12,
            "clamp at last content line"
        );
    }

    #[test]
    fn minus_goes_count_lines_up_to_first_non_blank() {
        assert_eq!(
            target(B, 12, Motion::UpFirstNonBlank, 1),
            10,
            "- -> line 2 'd'"
        );
        assert_eq!(
            target(B, 12, Motion::UpFirstNonBlank, 3),
            0,
            "3- -> line 0 'a'"
        );
        // Past the first line clamps to line 0.
        assert_eq!(
            target(B, 12, Motion::UpFirstNonBlank, 99),
            0,
            "clamp at first line"
        );
    }

    #[test]
    fn underscore_is_count_minus_one_lines_down() {
        // `_` alone == `^` (first non-blank of the current line), no vertical move.
        assert_eq!(
            target(B, 5, Motion::LineUnderscore, 1),
            target(B, 5, Motion::LineFirstNonBlank, 1),
            "_ == ^"
        );
        // `2_` moves one line down; `3_` two lines down.
        assert_eq!(
            target(B, 4, Motion::LineUnderscore, 2),
            10,
            "2_ -> one line down"
        );
        assert_eq!(
            target(B, 0, Motion::LineUnderscore, 3),
            10,
            "3_ -> two lines down"
        );
    }
}

#[cfg(test)]
mod tag_object_tests {
    //! Vim `it`/`at` — HTML/XML tag text objects (nesting-aware byte scan).
    use super::{char_span, Motion};

    fn tag(around: bool) -> Motion {
        Motion::Tag { around }
    }

    #[test]
    fn inner_and_around_tag() {
        let b = b"<div>hello</div>"; // content "hello" at [5,10); whole at [0,16)
        assert_eq!(char_span(b, 6, tag(false), 1), (5, 10), "it = content");
        assert_eq!(char_span(b, 6, tag(true), 1), (0, 16), "at = incl. tags");
        // Cursor ON a tag still resolves the block.
        assert_eq!(char_span(b, 1, tag(false), 1), (5, 10));
    }

    #[test]
    fn nesting_picks_the_innermost() {
        let b = b"<a><b>x</b></a>";
        assert_eq!(
            char_span(b, 6, tag(false), 1),
            (6, 7),
            "it = innermost content 'x'"
        );
        assert_eq!(
            char_span(b, 6, tag(true), 1),
            (3, 11),
            "at = innermost '<b>x</b>'"
        );
    }

    #[test]
    fn no_enclosing_tag_is_a_noop() {
        assert_eq!(char_span(b"plain text", 3, tag(false), 1), (3, 3));
        // A self-closing tag has no content to enclose the cursor.
        assert_eq!(char_span(b"<br/>x", 5, tag(true), 1), (5, 5));
    }

    #[test]
    fn count_climbs_outward_one_level_per_count() {
        let b = b"<a><b>x</b></a>"; // <a>[0,15), <b>[3,11), content 'x' at [6,7)
        assert_eq!(char_span(b, 6, tag(false), 1), (6, 7), "1it = innermost");
        assert_eq!(char_span(b, 6, tag(false), 2), (3, 11), "2it = <a>'s inner");
        assert_eq!(char_span(b, 6, tag(true), 2), (0, 15), "2at = whole <a>");
        // Count beyond the nesting depth is a no-op (Vim).
        assert_eq!(char_span(b, 6, tag(false), 3), (6, 6));
    }
}

#[cfg(test)]
mod sentence_motion_tests {
    //! Vim `)` / `(` — forward/backward to a sentence start.
    use super::{char_span, target, Motion};

    #[test]
    fn sentence_forward_and_back_find_starts() {
        let b = b"Hello. World! Foo?"; // starts at 0 (Hello), 7 (World), 14 (Foo)
        assert_eq!(target(b, 0, Motion::SentenceFwd, 1), 7);
        assert_eq!(target(b, 7, Motion::SentenceFwd, 1), 14);
        assert_eq!(
            target(b, 14, Motion::SentenceFwd, 1),
            18,
            "past the last → buffer end"
        );
        // `(` from mid-sentence → that sentence's start; from a start → the previous start.
        assert_eq!(target(b, 10, Motion::SentenceBack, 1), 7);
        assert_eq!(target(b, 7, Motion::SentenceBack, 1), 0);
        // Counted: two sentences forward.
        assert_eq!(target(b, 0, Motion::SentenceFwd, 2), 14);
    }

    #[test]
    fn d_close_paren_spans_to_next_sentence() {
        // `d)` from the start deletes up to (exclusive) the next sentence: "Hello. " → "World! Foo?".
        assert_eq!(
            char_span(b"Hello. World! Foo?", 0, Motion::SentenceFwd, 1),
            (0, 7)
        );
    }
}

#[cfg(test)]
mod windowed_grapheme_tests {
    //! The line-windowed `next_grapheme`/`prev_grapheme` must be byte-for-byte equivalent to segmenting the
    //! WHOLE buffer (the previous implementation), including across line breaks and over multi-byte clusters.
    use super::{next_boundary, next_grapheme, prev_boundary, prev_grapheme};
    use unicode_segmentation::GraphemeCursor;

    // The pre-refactor whole-buffer implementations, kept here as the oracle.
    fn next_whole(b: &[u8], pos: usize) -> usize {
        match std::str::from_utf8(b) {
            Ok(s) => {
                let mut gc = GraphemeCursor::new(pos.min(s.len()), s.len(), true);
                gc.next_boundary(s, 0)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| next_boundary(b, pos))
            }
            Err(_) => next_boundary(b, pos),
        }
    }
    fn prev_whole(b: &[u8], pos: usize) -> usize {
        match std::str::from_utf8(b) {
            Ok(s) => {
                let mut gc = GraphemeCursor::new(pos.min(s.len()), s.len(), true);
                gc.prev_boundary(s, 0)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| prev_boundary(b, pos))
            }
            Err(_) => prev_boundary(b, pos),
        }
    }

    fn assert_equivalent(text: &str) {
        let b = text.as_bytes();
        // Real callers only ever pass a char boundary (the cursor is always snapped), which is what
        // `GraphemeCursor` requires — so sweep those positions plus the end.
        let boundaries = text
            .char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(b.len()));
        for pos in boundaries {
            assert_eq!(
                next_grapheme(b, pos),
                next_whole(b, pos),
                "next_grapheme mismatch at pos {pos} in {text:?}"
            );
            assert_eq!(
                prev_grapheme(b, pos),
                prev_whole(b, pos),
                "prev_grapheme mismatch at pos {pos} in {text:?}"
            );
        }
    }

    #[test]
    fn windowed_matches_whole_buffer_across_lines_and_clusters() {
        assert_equivalent("abc\ndef\nghi"); // plain ASCII, multiple lines
        assert_equivalent("a👨‍👩‍👧b\ncd"); // ZWJ emoji family mid-line, then a line break
        assert_equivalent("é\ncafé\n"); // combining/precomposed + trailing newline
        assert_equivalent("한\n글자\nx"); // multi-byte CJK on several lines
        assert_equivalent(""); // empty buffer
        assert_equivalent("\n\n"); // blank lines only
        assert_equivalent("e\u{0301}x\ny"); // base + combining acute (a real 2-codepoint cluster)
    }

    #[test]
    fn crossing_a_newline_still_steps_one_cluster() {
        // `l` at the last char of a line lands on the newline; a further `l` crosses to the next line start.
        let b = b"ab\ncd";
        assert_eq!(next_grapheme(b, 1), 2, "b → the newline position");
        assert_eq!(next_grapheme(b, 2), 3, "newline → next line start");
        // `h` at a line start crosses back over the newline.
        assert_eq!(prev_grapheme(b, 3), 2, "line start → the newline");
        assert_eq!(prev_grapheme(b, 2), 1, "newline → prev line's last char");
    }
}

#[cfg(test)]
mod steps_tests {
    //! #1224, #1232: [`Steps`] reads a bounded window around a position; its answers must equal segmenting
    //! the whole line into its cells, for any order of queries over any bytes (bytes that are not UTF-8,
    //! positions inside a char, CRLF, combining marks, ZWJ and flag sequences longer than the first window),
    //! and a word walk over a long line must cost the text it crosses.
    use super::{
        char_len_at, inside_char, line_end, line_start, next_boundary, prev_boundary, target,
        Motion, Steps,
    };
    use proptest::prelude::*;
    use unicode_segmentation::UnicodeSegmentation;

    /// The reference cells of the line `[ls, le)`: each byte that belongs to no valid char alone, and each
    /// maximal run of valid chars cut into extended grapheme clusters as a string of its own. Returns the
    /// cell starts.
    fn cell_starts(b: &[u8], ls: usize, le: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let mut i = ls;
        while i < le {
            if char_len_at(b, i).is_none() {
                out.push(i);
                i += 1;
                continue;
            }
            let mut j = i;
            while j < le {
                match char_len_at(b, j) {
                    Some(w) => j += w,
                    None => break,
                }
            }
            let run = std::str::from_utf8(&b[i..j]).expect("a run of valid chars");
            out.extend(run.grapheme_indices(true).map(|(k, _)| i + k));
            i = j;
        }
        out
    }

    /// The reference `next_grapheme`: the first cell start after `pos` on its line, else the line end;
    /// the char step at a line's end, past the buffer, or inside a char.
    fn next_ref(b: &[u8], pos: usize) -> usize {
        if pos >= b.len() || b[pos] == b'\n' || inside_char(b, pos) {
            return next_boundary(b, pos);
        }
        let (ls, le) = (line_start(b, pos), line_end(b, pos));
        cell_starts(b, ls, le)
            .into_iter()
            .find(|&s| s > pos)
            .unwrap_or(le)
    }

    /// The reference `prev_grapheme`: the last cell start before `pos` on its line; the char step at a
    /// line's start or inside a char.
    fn prev_ref(b: &[u8], pos: usize) -> usize {
        let pos = pos.min(b.len());
        if pos == 0 || b[pos - 1] == b'\n' || inside_char(b, pos) {
            return prev_boundary(b, pos);
        }
        let ls = line_start(b, pos);
        cell_starts(b, ls, pos)
            .into_iter()
            .next_back()
            .expect("a cell before pos")
    }

    /// Text pieces that exercise the segmenter and the fallbacks.
    const PIECES: &[&[u8]] = &[
        b"a",
        b"_",
        b" ",
        b".",
        b"\t",
        b"\n",
        b"\r",
        b"\r\n",
        "e\u{301}".as_bytes(),
        "\u{301}".as_bytes(),
        "가".as_bytes(),
        "👨\u{200d}👩\u{200d}👧".as_bytes(),
        "\u{200d}".as_bytes(),
        "🇰🇷".as_bytes(),
        "🇯".as_bytes(),
        "👍🏽".as_bytes(),
        "\u{915}\u{94d}\u{937}".as_bytes(), // Devanagari conjunct (InCB)
        "\u{600}".as_bytes(),               // a Prepend char: joins the ASCII char after it
        b"\xff",
        b"\x80",
        b"\xe0\x80",
        b"\xea\xb0", // a cut 3-byte char
        b"\xed\xa0\x80", // a surrogate: three bytes that are not UTF-8
        b"\xc0\xaf",     // an overlong `/`
        // Clusters longer than the first window: the segmenter asks for more context.
        "👨\u{200d}👩\u{200d}👧\u{200d}👦👨\u{200d}👩\u{200d}👧\u{200d}👦👨\u{200d}👩\u{200d}👧\u{200d}👦".as_bytes(),
        "a\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}".as_bytes(),
        "🇰🇷🇯🇵🇺🇸🇰🇷🇯🇵🇺🇸🇰🇷🇯🇵🇺🇸".as_bytes(), // nine flags: a prev step counts back over them
    ];

    fn bytes(picks: &[usize]) -> Vec<u8> {
        picks
            .iter()
            .flat_map(|&i| PIECES[i % PIECES.len()].to_vec())
            .collect()
    }

    proptest! {
        #![proptest_config(crate::test_cases(512))]

        /// One `Steps` answers like the whole-line cells at every offset (char boundary or not, and one past the
        /// end), queried in an arbitrary order that jumps between lines and directions.
        #[test]
        fn steps_equal_the_whole_line_cells(
            picks in proptest::collection::vec(0usize..64, 0..40),
            order in proptest::collection::vec((0usize..400, any::<bool>()), 0..80),
        ) {
            let b = bytes(&picks);
            let steps = Steps::new(&b);
            let all = (0..=b.len() + 1).map(|p| (p, true)).chain((0..=b.len() + 1).map(|p| (p, false)));
            let jumps = order.iter().map(|&(p, f)| (p % (b.len() + 2), f));
            for (pos, fwd) in all.chain(jumps) {
                if fwd {
                    prop_assert_eq!(steps.next(pos), next_ref(&b, pos), "next at {} in {:?}", pos, b);
                } else {
                    prop_assert_eq!(steps.prev(pos), prev_ref(&b, pos), "prev at {} in {:?}", pos, b);
                }
                let p = pos.min(b.len());
                prop_assert_eq!(steps.line_start(pos), line_start(&b, p));
                prop_assert_eq!(steps.line_end(pos), line_end(&b, p));
            }
            // A forward and a backward walk, each step from the last landing, as the word loops do.
            let mut p = 0;
            while p < b.len() {
                let q = steps.next(p);
                prop_assert_eq!(q, next_ref(&b, p));
                p = q;
            }
            while p > 0 {
                let q = steps.prev(p);
                prop_assert_eq!(q, prev_ref(&b, p));
                p = q;
            }
        }
    }

    proptest! {
        #![proptest_config(crate::test_cases(2048))]

        /// The hand-rolled UTF-8 char check equals the standard library's on any bytes.
        #[test]
        fn char_len_at_equals_std(b in proptest::collection::vec(any::<u8>(), 1..6)) {
            let head = &b[..b.len().min(4)];
            let std_len = match std::str::from_utf8(head) {
                Ok(s) => s.chars().next().map(char::len_utf8),
                Err(e) if e.valid_up_to() > 0 => {
                    std::str::from_utf8(&head[..e.valid_up_to()]).ok().and_then(|s| s.chars().next()).map(char::len_utf8)
                }
                Err(_) => None,
            };
            prop_assert_eq!(char_len_at(&b, 0), std_len, "{:x?}", b);
        }
    }

    /// #1232, the cells nvim v0.12.6 steps (`l` from the line start, then `h` back from its end; captured
    /// headless, `nvim_win_get_cursor` byte columns): a byte that is not UTF-8 is one cell, and a
    /// combining mark or ZWJ sequence next to it still groups with its own valid text.
    #[test]
    fn invalid_bytes_are_cells_of_their_own() {
        let walk = |b: &[u8]| {
            let s = Steps::new(b);
            let mut fwd = vec![0];
            while *fwd.last().unwrap() < b.len() {
                fwd.push(s.next(*fwd.last().unwrap()));
            }
            let mut back = vec![b.len()];
            while *back.last().unwrap() > 0 {
                back.push(s.prev(*back.last().unwrap()));
            }
            (fwd, back)
        };
        // `é ÿ é` with a decomposed é and a lone 0xFF: l lands on 3, 4, 5, 6.
        let (fwd, back) = walk(b"e\xcc\x81 \xff e\xcc\x81");
        assert_eq!(fwd, [0, 3, 4, 5, 6, 9]);
        assert_eq!(back, [9, 6, 5, 4, 3, 0]);
        // A combining mark after the invalid byte does not join it: `\xff\u{301}a` is three cells.
        assert_eq!(walk(b"\xff\xcc\x81a").0, [0, 1, 3, 4]);
        // A cut char is one cell per byte, and a stray continuation byte is a cell (and a char boundary).
        assert_eq!(walk(b"\xea\xb0a\xcc\x81").0, [0, 1, 2, 5]);
        assert_eq!(walk(b"a\x80\xcc\x81b").0, [0, 1, 2, 4, 5]);
        assert!(super::is_boundary(b"a\x80", 1));
        assert!(!super::is_boundary("é".as_bytes(), 1));
        // A ZWJ family beside an invalid byte stays one cell.
        let fam = "👨\u{200d}👩".as_bytes();
        let b = [fam, b" \xff ", fam].concat();
        assert_eq!(walk(&b).0, [0, 11, 12, 13, 14, 25]);
    }

    /// Word motions over one 512 KiB line cross it in one pass: before #1224 every cluster re-validated the
    /// whole line, so these counts ran for hours; now they finish in well under a second even unoptimized.
    /// The landings are checked against the word count of one repeated unit.
    #[test]
    fn word_walks_over_a_long_line_are_linear() {
        const UNIT: &str = "ab_c, d.e(\"가나\") e\u{301}x ";
        let reps = (512 << 10) / UNIT.len();
        let b = UNIT.repeat(reps).into_bytes();
        let unit = UNIT.len();
        // Words per unit, from a short buffer: the `w` count that lands on the second unit's start.
        let short = UNIT.repeat(3).into_bytes();
        let per = (1..100)
            .find(|&k| target(&short, 0, Motion::WordFwd, k) == unit)
            .expect("the unit is a whole number of words");
        let k = (reps - 2) as u32;
        assert_eq!(target(&b, 0, Motion::WordFwd, per * k), k as usize * unit);
        let back = target(&b, (reps - 1) * unit, Motion::WordBack, per * k);
        assert_eq!(back, unit, "`b` walks back as many units");
        let e = target(&b, 0, Motion::WordEnd, per * k);
        assert!(
            e > (k as usize - 1) * unit && e < k as usize * unit,
            "`e` ends in unit {k}"
        );
        let ge = target(&b, (reps - 1) * unit, Motion::WordEndBack, per * k);
        assert!(ge > unit && ge < 2 * unit, "`ge` ends in the second unit");
        // A hostile `l` count walks the line once and stops at its edge (the caller clamps it onto a char).
        assert_eq!(target(&b, 0, Motion::Right, u32::MAX), b.len());
    }
}

#[cfg(test)]
mod method_motion_tests {
    //! Vim `]m`/`[m`/`]M`/`[M` — method (brace-block) motions. Every expected byte offset is pinned to
    //! nvim v0.12.4 (captured headless: cursor set by byte offset, `normal! <keys>`, cursor read back as a
    //! byte offset). A faithful port of Vim's `nv_bracket_block`; see `method_target`.
    use super::{target, Motion};

    // A class with two methods. Byte map (nvim-verified landing offsets reference these):
    //   byte  0 R1: "class Foo {"      → open-brace at byte 10
    //   byte 12 R2: "    void a() {"   → open-brace at byte 25
    //   byte 27 R3: "        x;"       → `x` at byte 35
    //   byte 38 R4: "    }"            → close-brace at byte 42
    //   byte 44 R5: "    void b() {"   → open-brace at byte 57
    //   byte 59 R6: "        y;"
    //   byte 70 R7: "    }"            → close-brace at byte 74
    //   byte 76 R8: "}"                → close-brace at byte 76
    const CLASS: &[u8] =
        b"class Foo {\n    void a() {\n        x;\n    }\n    void b() {\n        y;\n    }\n}\n";
    // A flat sequence of top-level `{}` blocks.
    //   byte  0 R1: "void a() {"  → open-brace at byte 9
    //   byte 11 R2: "    x;"
    //   byte 18 R3: "}"           → close-brace at byte 18
    //   byte 20 R4: "void b() {"  → open-brace at byte 29
    //   byte 38 R6: "}"
    const FLAT: &[u8] = b"void a() {\n    x;\n}\nvoid b() {\n    y;\n}\n";

    fn t(b: &[u8], cur: usize, m: Motion, count: u32) -> usize {
        target(b, cur, m, count)
    }

    #[test]
    fn class_forward_start_walks_every_open_then_the_top_close() {
        // `]m` visits each open-brace in forward order, then (no more open-brace) the class's top-level close-brace.
        assert_eq!(
            t(CLASS, 0, Motion::MethodStartFwd, 1),
            10,
            "]m → class open-brace"
        );
        assert_eq!(
            t(CLASS, 0, Motion::MethodStartFwd, 2),
            25,
            "2]m → method a open-brace"
        );
        assert_eq!(
            t(CLASS, 0, Motion::MethodStartFwd, 3),
            57,
            "3]m → method b open-brace"
        );
        assert_eq!(
            t(CLASS, 0, Motion::MethodStartFwd, 4),
            76,
            "4]m → class close close-brace"
        );
        // A count past the last boundary fails the whole motion → no move (Vim beeps).
        assert_eq!(t(CLASS, 0, Motion::MethodStartFwd, 5), 0, "5]m → no-op");
    }

    #[test]
    fn class_forward_end_walks_every_close_then_stops() {
        // `]M` visits each method-end close-brace, plus the top-level open-brace as the class boundary.
        assert_eq!(
            t(CLASS, 0, Motion::MethodEndFwd, 1),
            10,
            "]M → class open-brace (class boundary)"
        );
        assert_eq!(
            t(CLASS, 0, Motion::MethodEndFwd, 2),
            42,
            "2]M → method a close-brace"
        );
    }

    #[test]
    fn from_inside_a_method_both_directions() {
        // Cursor on `x` (byte 35), inside method a's body.
        assert_eq!(
            t(CLASS, 35, Motion::MethodStartFwd, 1),
            57,
            "]m → method b open-brace"
        );
        assert_eq!(
            t(CLASS, 35, Motion::MethodStartBack, 1),
            25,
            "[m → method a open-brace"
        );
        assert_eq!(
            t(CLASS, 35, Motion::MethodEndFwd, 1),
            42,
            "]M → method a close-brace"
        );
        assert_eq!(
            t(CLASS, 35, Motion::MethodEndBack, 1),
            10,
            "[M → class open-brace"
        );
        assert_eq!(
            t(CLASS, 35, Motion::MethodStartBack, 2),
            10,
            "2[m → class open-brace"
        );
    }

    #[test]
    fn flat_top_level_blocks() {
        // Flat blocks: `]m` reaches the first open-brace, then that block's top-level close-brace, then fails (Vim treats
        // the level-0 close-brace as end-of-class, so it does not roll on into the next block).
        assert_eq!(
            t(FLAT, 0, Motion::MethodStartFwd, 1),
            9,
            "]m → first open-brace"
        );
        assert_eq!(
            t(FLAT, 0, Motion::MethodStartFwd, 2),
            18,
            "2]m → its top-level close-brace"
        );
        assert_eq!(
            t(FLAT, 0, Motion::MethodStartFwd, 3),
            0,
            "3]m → no-op (past class end)"
        );
        assert_eq!(
            t(FLAT, 0, Motion::MethodEndFwd, 1),
            9,
            "]M → first open-brace (class boundary)"
        );
    }

    #[test]
    fn no_braces_is_a_noop() {
        let b = b"hello world\nno braces here\n";
        assert_eq!(t(b, 0, Motion::MethodStartFwd, 1), 0);
        assert_eq!(t(b, 0, Motion::MethodEndBack, 1), 0);
        assert_eq!(t(b, 5, Motion::MethodStartBack, 1), 5);
    }
}

#[cfg(test)]
mod line_pass_tests {
    //! #1231: the word helpers, `fails` and the line-end clamps tell a line's ends by the bytes around a
    //! position instead of finding the line. Each must answer exactly as the line-finding version it
    //! replaced, copied here verbatim (`old_*`), on any bytes and at any position.
    use super::*;
    use proptest::prelude::*;

    /// The pre-#1231 `off_phantom_at`, verbatim.
    fn old_off_phantom_at(b: &[u8], pos: usize) -> usize {
        if pos < b.len() || b.last() != Some(&b'\n') {
            return pos;
        }
        let le = b.len() - 1;
        if le > line_start(b, le) {
            prev_grapheme(b, le)
        } else {
            le
        }
    }

    fn old_vcls(s: &Steps, p: usize, big: bool) -> u32 {
        if p >= s.line_end(p) {
            return 0;
        }
        char_cls(s.b, p, big)
    }

    /// Whether `p` is on an empty line (Vim `col == 0 && LINEEMPTY`).
    fn old_on_empty_line(s: &Steps, p: usize) -> bool {
        let ls = s.line_start(p);
        p == ls && s.line_end(ls) == ls
    }

    /// Vim `inc()`: the next position and how it got there (0 within the line, 2 onto the line's NUL slot, 1 to
    /// the next line's start), or `None` from the last line's NUL slot.
    fn old_vinc(s: &Steps, p: usize) -> Option<(usize, u8)> {
        let le = s.line_end(p);
        if p < le {
            let q = s.next(p);
            Some((q, if q < le { 0 } else { 2 }))
        } else if le + 1 < s.b.len() {
            Some((le + 1, 1))
        } else {
            None
        }
    }

    /// Vim `dec()`: the previous position (0 within the line, 1 back onto the previous line's NUL slot), or
    /// `None` at the buffer start.
    fn old_vdec(s: &Steps, p: usize) -> Option<(usize, u8)> {
        if p > s.line_start(p) {
            Some((s.prev(p), 0))
        } else if p > 0 {
            Some((p - 1, 1))
        } else {
            None
        }
    }

    fn old_word_fwd(s: &Steps, pos: usize, big: bool, count: u32, eol: bool) -> usize {
        let b = s.b;
        let mut p = pos.min(b.len());
        for left in (0..count.max(1)).rev() {
            let stop = |i: u8| i >= 1 && eol && left == 0;
            let sclass = old_vcls(s, p, big);
            let last_line = s.line_end(p) + 1 >= b.len();
            let Some((q, i)) = old_vinc(s, p) else {
                return p;
            };
            p = q;
            if (i >= 1 && last_line) || stop(i) {
                return p;
            }
            if sclass != 0 {
                while old_vcls(s, p, big) == sclass {
                    let Some((q, i)) = old_vinc(s, p) else {
                        return p;
                    };
                    p = q;
                    if stop(i) {
                        return p;
                    }
                }
            }
            while old_vcls(s, p, big) == 0 {
                if old_on_empty_line(s, p) {
                    break;
                }
                let Some((q, i)) = old_vinc(s, p) else {
                    return p;
                };
                p = q;
                if stop(i) {
                    return p;
                }
            }
        }
        p
    }

    #[allow(clippy::too_many_lines)]
    fn old_fails(b: &[u8], cursor: usize, m: Motion, count: u32, op: bool, visual: bool) -> bool {
        let n = count.max(1);
        let cur = cursor.min(b.len());
        let ls = line_start(b, cur);
        let le = line_end(b, cur);
        let last_line = le >= content_end(b);
        // Where the motion leaves an on-char caret: never on a non-empty line's break or the phantom slot.
        let clamp = |t: usize| {
            let t = old_off_phantom_at(b, t.min(b.len()));
            let (tls, tle) = (line_start(b, t), line_end(b, t));
            if t == tle && tle > tls {
                prev_grapheme(b, tle)
            } else {
                t
            }
        };
        let still = || clamp(target(b, cur, m, count)) == clamp(cur);
        match m {
            Motion::Left => !op && cur <= ls,
            Motion::Right if visual => !op && cur >= le,
            Motion::Right => !op && next_grapheme(b, cur) >= le,
            Motion::WrapLeft | Motion::WrapRight => !op && still(),
            Motion::Up | Motion::UpFirstNonBlank => ls == 0,
            Motion::Down | Motion::DownFirstNonBlank => last_line,
            Motion::WordFwd | Motion::BigWordFwd => !op && still(),
            Motion::WordBack
            | Motion::BigWordBack
            | Motion::WordEndBack
            | Motion::BigWordEndBack => still(),
            Motion::WordEnd | Motion::BigWordEnd => {
                !op && {
                    let before = if n > 1 { target(b, cur, m, n - 1) } else { cur };
                    clamp(target(b, cur, m, n)) == clamp(before)
                }
            }
            Motion::FindChar {
                ch,
                forward,
                till,
                repeat,
            } => find_char_target(b, cur, ch, forward, till, repeat, n).is_none(),
            Motion::ParagraphFwd | Motion::ParagraphBack => {
                paragraph_landing(b, cur, m == Motion::ParagraphFwd, n).is_none()
            }
            Motion::SentenceFwd | Motion::SentenceBack => {
                crate::textobj::sentence_motion_fails(b, cur, m == Motion::SentenceFwd, n)
            }
            Motion::MatchBracket => match_bracket(b, cur).is_none(),
            Motion::InnerParagraph | Motion::AParagraph => {
                paragraph_fails(b, cur, n, m == Motion::AParagraph)
            }
            Motion::UnmatchedParenBack
            | Motion::UnmatchedParenFwd
            | Motion::UnmatchedBraceBack
            | Motion::UnmatchedBraceFwd => still(),
            // `{count}$` moves `count - 1` lines down first, failing like `j` only on the last line (`5$` on
            // the line above it still reaches the last line's end).
            Motion::LineEnd => n > 1 && last_line,
            // `{count}dd`/`yy`/`cc`/`>>` move `count - 1` lines down first (`nv_lineop` → `cursor_down`), so on
            // the last line a count above 1 fails like `j` there (#1176).
            Motion::Line => op && n > 1 && last_line,
            // `it`/`at` outside a tag pair (or a count past its nesting) cancel the operator (#1179). The `at`
            // span holds the tags themselves, so it is empty only when there is no pair.
            Motion::Tag { .. } => {
                op && {
                    let (s, e) = tag_span(b, cur, true, n);
                    s == e
                }
            }
            _ => false,
        }
    }

    /// Bytes that exercise the line model: blank and empty lines, CR, words, punctuation, multibyte and
    /// combining text, a bracket pair and an invalid byte.
    fn text() -> impl Strategy<Value = Vec<u8>> {
        let piece = prop_oneof![
            Just(b"a".to_vec()),
            Just(b"_".to_vec()),
            Just(b" ".to_vec()),
            Just(b".".to_vec()),
            Just(b"\n".to_vec()),
            Just(b"\n\n".to_vec()),
            Just(b"\r".to_vec()),
            Just(b"(".to_vec()),
            Just(b")".to_vec()),
            Just("가".as_bytes().to_vec()),
            Just("e\u{301}".as_bytes().to_vec()),
            Just(b"\xff".to_vec()),
        ];
        proptest::collection::vec(piece, 0..24).prop_map(|v| v.concat())
    }

    fn motions() -> Vec<Motion> {
        use Motion::*;
        vec![
            Left,
            Right,
            WrapLeft,
            WrapRight,
            Up,
            Down,
            UpFirstNonBlank,
            DownFirstNonBlank,
            WordFwd,
            BigWordFwd,
            WordBack,
            BigWordBack,
            WordEnd,
            BigWordEnd,
            WordEndBack,
            BigWordEndBack,
            LineEnd,
            Line,
            MatchBracket,
            SentenceFwd,
            UnmatchedParenFwd,
            UnmatchedParenBack,
        ]
    }

    proptest! {
        #![proptest_config(crate::test_cases(256))]

        /// The byte tests equal the line-finding ones, at every position and one past the end.
        #[test]
        fn line_end_tests_equal_finding_the_line(b in text()) {
            let s = Steps::new(&b);
            for p in 0..=b.len() + 1 {
                prop_assert_eq!(at_nul(&b, p), p >= line_end(&b, p), "at_nul {}", p);
                if p <= b.len() {
                    prop_assert_eq!(at_bol(&b, p), p == line_start(&b, p), "at_bol {}", p);
                }
                prop_assert_eq!(vcls(&s, p, false), old_vcls(&s, p, false), "vcls {}", p);
                prop_assert_eq!(vcls(&s, p, true), old_vcls(&s, p, true), "vCLS {}", p);
                prop_assert_eq!(on_empty_line(&s, p), old_on_empty_line(&s, p), "empty {}", p);
                prop_assert_eq!(vinc(&s, p), old_vinc(&s, p), "inc {}", p);
                prop_assert_eq!(vdec(&s, p), old_vdec(&s, p), "dec {}", p);
                prop_assert_eq!(off_phantom_at(&b, p), old_off_phantom_at(&b, p), "phantom {}", p);
                // The planner's Normal-mode clamp onto a non-empty line's last char.
                let old = p == line_end(&b, p) && line_end(&b, p) > line_start(&b, p);
                let new = p <= b.len() && at_nul(&b, p) && !at_bol(&b, p);
                prop_assert_eq!(new, old, "clamp {}", p);
            }
        }

        /// `w`/`W`, bare and under an operator, with any count, equal the pre-#1231 walk.
        #[test]
        fn word_fwd_equals_the_line_finding_walk(b in text(), count in 1u32..4) {
            let s = Steps::new(&b);
            for p in 0..=b.len() {
                for big in [false, true] {
                    for eol in [false, true] {
                        prop_assert_eq!(
                            word_fwd(&s, p, big, count, eol),
                            old_word_fwd(&s, p, big, count, eol),
                            "w at {} big={} eol={} count={}", p, big, eol, count
                        );
                    }
                }
            }
        }

        /// `fails` (sharing the target through its memo) equals the pre-#1231 one for every motion, from
        /// every char boundary, bare, under an operator and in Visual.
        #[test]
        fn fails_equals_the_line_finding_check(b in text(), count in 0u32..4) {
            for p in (0..=b.len()).filter(|&p| is_boundary(&b, p)) {
                for m in motions() {
                    for (op, visual) in [(false, false), (true, false), (false, true)] {
                        let memo = std::cell::OnceCell::new();
                        let new = fails_with(&b, p, m, count, op, visual, &memo);
                        prop_assert_eq!(
                            new,
                            old_fails(&b, p, m, count, op, visual),
                            "{:?} at {} count={} op={} visual={}", m, p, count, op, visual
                        );
                        if let Some(&t) = memo.get() {
                            prop_assert_eq!(t, target(&b, p, m, count), "memo {:?} at {}", m, p);
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod find_char_tests {
    //! #1232: `f`/`t`/`F`/`T` walk the cells from the cursor instead of segmenting the whole line. On
    //! valid UTF-8 they must answer as the line-segmenting version they replaced, copied here verbatim.
    use super::*;
    use proptest::prelude::*;

    /// The pre-#1232 `find_char_target`, verbatim.
    fn old_find_char_target(
        b: &[u8],
        cursor: usize,
        ch: char,
        forward: bool,
        till: bool,
        repeat: bool,
        count: u32,
    ) -> Option<usize> {
        let n = count.max(1) as usize;
        let ls = line_start(b, cursor);
        let le = line_end(b, cursor); // newline / end position for this line
        let line = std::str::from_utf8(b.get(ls..le)?).ok()?;
        // A target matches the FIRST char of a cluster only: `f` to a combining mark, an emoji modifier or the
        // second flag letter finds nothing inside a cluster, as nvim v0.12.6 (`f\u{301}` on `xáb` fails; #909).
        let starts = line
            .grapheme_indices(true)
            .filter_map(|(i, g)| g.chars().next().map(|c| (ls + i, c)));
        // `t`/`T` stop one CHARACTER short of the match: a base with its marks, a flag, a ZWJ sequence (#909).
        let skip = (repeat && till && n == 1).then(|| {
            if forward {
                next_grapheme(b, cursor)
            } else {
                prev_grapheme(b, cursor)
            }
        });
        if forward {
            // Matches strictly after the cursor, in order; take the n-th.
            let hit = starts
                .filter(|&(pos, c)| pos > cursor && c == ch && Some(pos) != skip)
                .nth(n - 1)?
                .0;
            Some(if till { prev_grapheme(b, hit) } else { hit })
        } else {
            // Matches strictly before the cursor; take the n-th counting backward.
            let hit = starts
                .filter(|&(pos, c)| pos < cursor && c == ch && Some(pos) != skip)
                .rev()
                .nth(n - 1)?
                .0;
            Some(if till { next_grapheme(b, hit) } else { hit })
        }
    }

    proptest! {
        #![proptest_config(crate::test_cases(256))]

        /// Every char search, from every char boundary, with any count, equals the old one on valid text.
        #[test]
        fn find_char_equals_the_line_segmenting_search(
            text in "[ab \n\u{301}가👍🏽🇰🇷x]{0,20}",
            count in 1u32..4,
        ) {
            let b = text.as_bytes();
            for cur in (0..=b.len()).filter(|&p| text.is_char_boundary(p)) {
                for ch in ['a', 'b', ' ', '가', 'x', '\u{301}', '👍', '🇰'] {
                    for (forward, till, repeat) in [
                        (true, false, false),
                        (true, true, false),
                        (true, true, true),
                        (false, false, false),
                        (false, true, false),
                        (false, true, true),
                    ] {
                        prop_assert_eq!(
                            find_char_target(b, cur, ch, forward, till, repeat, count),
                            old_find_char_target(b, cur, ch, forward, till, repeat, count),
                            "{:?} at {} ch={:?} fwd={} till={} rep={} n={}",
                            text, cur, ch, forward, till, repeat, count
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod invalid_byte_tests {
    //! #1243: a byte that belongs to no valid UTF-8 char is one char, with the class of the Latin-1 char of
    //! its value (0xA0 blank, 0xB5 and 0xC0-0xFF keyword, the rest punctuation), for the column count and
    //! the keyword lookups, as nvim v0.12.6 reads it.
    use super::{col_of, keyword_before_cursor, keyword_completions, word_under_cursor};

    #[test]
    fn col_of_counts_each_invalid_byte_as_a_char() {
        assert_eq!(col_of(b"a\xff\xc3\xa9\xffb", 0, 6), 5);
        assert_eq!(col_of(b"\xea\xb0ab", 0, 2), 2);
        assert_eq!(col_of(b"x\x80\xcc\x81y", 0, 4), 3);
        assert_eq!(col_of("한글".as_bytes(), 0, 6), 2);
    }

    #[test]
    fn word_under_cursor_reads_the_latin1_class() {
        // 0xFF is a keyword char: it joins the ASCII letters around it.
        assert_eq!(word_under_cursor(b"foo\xffbar x", 0), Some((0, 7)));
        // 0x80 is punctuation: it ends the word, and a cursor on it finds the next one.
        assert_eq!(word_under_cursor(b"ab\x80cd", 0), Some((0, 2)));
        assert_eq!(word_under_cursor(b"\x80ab ab", 0), Some((1, 3)));
        // A cut sequence: its lead 0xE9 is a keyword char, its 0xB0 punctuation.
        assert_eq!(word_under_cursor(b"\xe9\xb0ab", 0), Some((0, 1)));
        assert_eq!(word_under_cursor(b"\x80 \xa0", 0), None);
    }

    #[test]
    fn keyword_before_cursor_reads_the_latin1_class() {
        assert_eq!(
            keyword_before_cursor(b"x foo\xffba", 9),
            (2, &b"foo\xffba"[..])
        );
        assert_eq!(keyword_before_cursor(b"foo\x80ba", 6), (4, &b"ba"[..]));
        // Candidates split at 0x80; one holding a byte that is not UTF-8 is offered spelled, as nvim
        // offers it (#1252).
        let b = b"foo\x80bar fo\xffx food\nfo";
        let ff = crate::pattern::spell(b"fo\xffx").into_owned();
        assert_eq!(
            keyword_completions(b, b.len(), b"fo"),
            vec!["foo", &ff, "food"]
        );
        assert_eq!(&*crate::pattern::unspell(&ff), b"fo\xffx");
        // <cword> looks for a keyword with the Latin-1 class: 0x80 is punctuation, so `x` is found.
        assert_eq!(
            super::cword_under_cursor(b"+\x80+ x", 0, false),
            Some((4, 5))
        );
        assert_eq!(
            super::cword_under_cursor(b"+ \xffa", 0, false),
            Some((2, 4))
        );
    }
}
