//! Semantic editor commands (INV-CMD-SEMANTIC) — the granularity at which edits are recorded as a trace.
//!
//! A keymap resolves keys onto these (the input engine, C2); a [`crate::trace::Trace`] is a list of them, so
//! it survives keymap changes. Each command has a stable line form (`to_line`/`from_line`) — a dependency-
//! free, human-readable serialization used by the trace file.

use crate::motion::Motion;

/// How a command-line search (`/pat`) folds into the editing grammar. A search is a MOTION (Vim: `/` is a
/// motion), so it can stand alone (`Move`) or be the range of an operator (`d/pat`, `c/pat`, `y/pat`). The
/// operator range is charwise-EXCLUSIVE — `[cursor, match)` — matching Vim's rule for `/` as a motion.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SearchOp {
    /// Bare `/pat` (and `{count}/pat`): move the cursor to the match.
    Move,
    /// `d/pat`: delete `[cursor, match)`.
    Delete,
    /// `c/pat`: delete `[cursor, match)` and enter Insert.
    Change,
    /// `y/pat`: yank `[cursor, match)`.
    Yank,
    /// `gu/pat` / `gU/pat` / `g~/pat` / `g?/pat` (and `gUn`, `gUgn`): recase the span.
    Case(WordCase),
    /// `>/pat` (`left: false`) / `</pat` (`left: true`): shift every line the span touches.
    Shift { left: bool },
    /// `=/pat`: reindent every line the span touches.
    Reindent,
    /// `gq/pat` (`keep: false`) / `gw/pat` (`keep: true`): reflow every line the span touches.
    Format { keep: bool },
    /// `gc/pat`: comment or uncomment every line the span touches.
    Comment,
}

impl SearchOp {
    /// The span operator this search folds into, for every form beyond the bare move and the d/c/y trio
    /// (which keep their own search-specific planning). Shared with [`MarkOp`] so a search, a `gn` match
    /// and a mark all apply case / shift / reindent / format through ONE planner (#800).
    #[must_use]
    pub fn span_op(self) -> Option<MarkOp> {
        match self {
            SearchOp::Move | SearchOp::Delete | SearchOp::Change | SearchOp::Yank => None,
            SearchOp::Case(c) => Some(MarkOp::Case(c)),
            SearchOp::Shift { left } => Some(MarkOp::Shift { left }),
            SearchOp::Reindent => Some(MarkOp::Reindent),
            SearchOp::Format { keep } => Some(MarkOp::Format { keep }),
            SearchOp::Comment => Some(MarkOp::Comment),
        }
    }
}

/// A Vim search offset — the `{offset}` after the closing `/` (forward) or `?` (backward) of a search
/// (`:help search-offset`). It relocates the landing away from the plain match start and, under an
/// operator, changes the motion's wiseness. Verified against nvim v0.12.4.
///
/// - [`None`](Self::None) — no offset (`/pat` or a bare trailing `/pat/`): land on the match's first
///   char; the operator motion is charwise-EXCLUSIVE (`[cursor, match_start)`).
/// - [`Start`](Self::Start) — `/pat/s±N` (and the synonym `/pat/b±N`): land `N` characters from the
///   match START (`s`/`b` alone = `Start(0)`, identical to [`None`] for the cursor landing). Still
///   EXCLUSIVE under an operator (only `e` turns it inclusive).
/// - [`End`](Self::End) — `/pat/e±N`: land `N` characters from the match's LAST char (`e` alone =
///   `End(0)`). Under an operator the motion becomes charwise-INCLUSIVE through the offset char.
/// - [`Line`](Self::Line) — `/pat/±N` (a bare signed/unsigned number, `+N`/`-N`/`N`): move `N` lines
///   below (positive) / above (negative) the match's line, cursor to COLUMN 0 (Vim "column 1" — NOT
///   the first non-blank; verified against nvim). Under an operator the motion is LINEWISE.
///
/// Character offsets count CHARACTERS and cross line boundaries (e.g. `/foo/e+1` at end-of-line lands
/// on the next line's first column), clamped to the buffer. `N` without a sign is positive (`e2`==`e+2`,
/// `s3`==`s+3`, `/foo/2`==`/foo/+2`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SearchOffset {
    /// No offset — land on the match start; exclusive under an operator.
    #[default]
    None,
    /// `s±N` / `b±N` — `N` chars from the match START. Exclusive under an operator.
    Start(i32),
    /// `e±N` — `N` chars from the match's LAST char. INCLUSIVE under an operator.
    End(i32),
    /// `±N` (bare number) — `N` lines from the match line, column 0. LINEWISE under an operator.
    Line(i32),
}

impl SearchOffset {
    /// Parse the raw `{offset}` suffix a user typed after the closing delimiter (`/pat/{THIS}` or
    /// `?pat?{THIS}`), per Vim's grammar: `[se b]?[+-]?\d*` (char offset, or start/end alone) OR
    /// `[+-]?\d+` / bare digits (line offset). An empty string is [`None`]. Returns `None` (the type's
    /// no-offset) for anything unrecognized rather than erroring — Vim likewise ignores a bad offset.
    #[must_use]
    pub fn parse(raw: &str) -> SearchOffset {
        let raw = raw.trim();
        if raw.is_empty() {
            return SearchOffset::None;
        }
        let mut chars = raw.chars();
        let first = raw.as_bytes()[0];
        // Character offset: a leading `e`, `s`, or `b`.
        if matches!(first, b'e' | b's' | b'b') {
            chars.next();
            let rest = chars.as_str();
            // `e`/`s`/`b` alone → offset 0. A trailing `[+-]?\d*` is the signed count (unsigned = +).
            let n = parse_signed_amount(rest).unwrap_or(0);
            return if first == b'e' {
                SearchOffset::End(n)
            } else {
                SearchOffset::Start(n)
            };
        }
        // Line offset: `[+-]?\d+`, or a lone `+`/`-` (== 0).
        match parse_signed_amount(raw) {
            Some(n) => SearchOffset::Line(n),
            None => SearchOffset::None,
        }
    }
}

/// Parse a Vim offset amount: an optional sign then digits. `""` → 0-with-explicit? No — an EMPTY string
/// yields `None` (the caller decides the default), a lone `+`/`-` yields `Some(0)`, `"2"` → `Some(2)`,
/// `"-3"` → `Some(-3)`, `"+1"` → `Some(1)`. A non-numeric tail yields `None`.
fn parse_signed_amount(s: &str) -> Option<i32> {
    if s.is_empty() {
        return None;
    }
    let (sign, digits) = match s.as_bytes()[0] {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => (1, s),
    };
    if digits.is_empty() {
        // A lone sign (`+`/`-`) means magnitude 0.
        return Some(0);
    }
    let mag: i32 = digits.parse().ok()?;
    Some(sign * mag)
}

/// Which operator a forced-wise motion applies ([`Command::OpForced`]). The plain operator commands are
/// [`Command::Delete`]/[`Command::Change`]/[`Command::Yank`]; this enum names the same three for the rarer
/// forced-wise form so that form needs no parallel command per operator.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OpKind {
    Delete,
    Change,
    Yank,
}

/// Which operator an [`Command::OpToMark`] applies over the cursor..mark span — a SUPERSET of [`OpKind`].
/// Besides delete/change/yank it carries the case (`` g~`a ``/`` gu`a ``/`` gU`a ``), shift (`` >`a ``/
/// `` <`a ``) and reindent (`` =`a ``) operators, which also take a named mark as their motion. It is a
/// distinct enum rather than a widened [`OpKind`] because [`OpKind`]'s other users ([`Command::OpForced`],
/// blockwise Visual) have no case/shift/reindent form — so widening would spread `unreachable!` arms there.
///
/// The `` ` `` (backtick) form and the `'` (quote) linewise form of `` d ``/`` c ``/`` y `` and case honour
/// the [`Command::OpToMark`] `linewise` flag; shift and reindent are ALWAYS linewise (Vim: `>`/`<`/`=`
/// operate on whole lines even over a charwise motion), so they ignore it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MarkOp {
    Delete,
    Change,
    Yank,
    /// `` g~`a ``/`` gu`a ``/`` gU`a `` — recase the span (toggle / lower / upper); honours `linewise`.
    Case(WordCase),
    /// `` >`a `` (`left: false`) / `` <`a `` (`left: true`) — shift the spanned lines one indent level.
    Shift {
        left: bool,
    },
    /// `` =`a `` — reindent the spanned lines to their bracket-depth level.
    Reindent,
    /// `` gq`a `` (`keep: false`) / `` gw`a `` (`keep: true`) — reflow the spanned lines to `'textwidth'`.
    Format {
        keep: bool,
    },
    /// `` gc`a `` — comment or uncomment the spanned lines with `'commentstring'`.
    Comment,
}

/// A Visual operator beyond `d`/`c`/`y`/`p`/`r`/case ([`Command::VisualOp`]). Each acts on the live
/// selection: most treat it as WHOLE LINES whatever its shape (`:help v_D`, `v_>`, `v_J`, `v_=`, `v_gq`).
/// Ground truth is nvim v0.12.6 (the `v_*`/`vb_*` oracle fixtures).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VisualOp {
    /// `D`/`X` (delete), `Y` (yank), `C`/`S`/`R` (change): the operator over every selected line, as a
    /// linewise span. Blockwise `S`/`R` are linewise too; blockwise `X`/`Y` are plain block `d`/`y`.
    Lines(OpKind),
    /// Blockwise `D`/`C`: the block from its left column to the END of every row (`:help v_b_D`).
    BlockToEol(OpKind),
    /// `>`/`<`: shift every selected line `count` indent levels (`left` for `<`).
    Shift { left: bool, count: u32 },
    /// `J` (`spaces`) / `gJ`: join the selected lines; a one-line selection joins it with the next.
    Join { spaces: bool },
    /// `=`: reindent the selected lines.
    Reindent,
    /// `gq`: reflow the selected lines.
    Format,
    /// `gc`: comment or uncomment the selected lines with `'commentstring'` (#618).
    Comment,
    /// Blockwise `O`: move to the other corner on the SAME row (the anchor takes the cursor's column).
    OtherCorner,
    /// `zy`: yank the selection like `y`, but without a block's trailing white space (`:help zy`).
    YankTrim,
    /// `:`/`!` (`:help v_:`): leave the selection with the cursor at its start, as Vim's colon operator
    /// does before it opens the command line on `'<,'>`.
    LeaveAtStart,
}

/// A motion's forced wise-ness (Vim `o_v` / `o_V` / `o_CTRL-V`): the operator applies the motion but
/// overrides whether the removed span is charwise, linewise, or blockwise. `Charwise` also toggles the
/// motion's exclusive/inclusive edge (Vim's rule for `v`). `Blockwise` treats the cursor and the motion
/// target as the two corners of a rectangle (`d<C-v>j` deletes one column across two rows).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ForcedWise {
    Charwise,
    Linewise,
    Blockwise,
}

/// The SHAPE of a Visual/Select selection (F-025 c1: selection shape is namespace state, not a boolean).
/// Charwise (`v`) spans a contiguous byte range; linewise (`V`) whole lines; blockwise (`CTRL-V`) a
/// column-aligned rectangle across lines. `v`/`V`/`CTRL-V` switch the shape or leave the namespace.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SelectKind {
    #[default]
    Charwise,
    Linewise,
    Blockwise,
}

/// A blockwise-Visual insert command: type on the block's top row, then REPLICATE the typed text down
/// every row of the block on `<Esc>`. `Insert` (`I`) inserts at the block's left edge; `Append` (`A`)
/// appends at the right edge (padding short lines); `Change` (`c`/`s`) deletes the block first, then
/// inserts at the left edge.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlockInsertKind {
    Insert,
    Append,
    Change,
}

/// Where an auto-indented open-line lands (F-015 Phase 2). `Below`/`Above` are `o`/`O` (open at end/start of
/// the current line); `Split` is `<CR>` in Insert (split at the cursor). The frontend rewrites the plain
/// `OpenBelow`/`OpenAbove`/`InsertNewline` into [`Command::OpenLineIndent`] with a tree-suggested level.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OpenKind {
    Below,
    Above,
    Split,
}

/// A semantic command — the granularity a trace records. Beyond single motions/edits it carries the editing
/// grammar: a counted move, and the `delete`/`change` operators over a motion range (`dw`, `d2w`, `cc`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Command {
    // motion
    MoveLeft,
    MoveRight,
    MoveUp,
    MoveDown,
    MoveLineStart,
    MoveLineEnd,
    // mode / insert-entry
    EnterInsert,
    EnterInsertAfter,
    EnterNormal,
    /// Enter Terminal mode (F-011): subsequent keys forward to the focused terminal buffer's PTY child. The
    /// frontend arms this when `:terminal` opens and on `i`/`a` from Terminal-Normal.
    EnterTerminal,
    /// Enter Terminal-Normal mode (F-011, `t_CTRL-\ CTRL-N`): read-only scrollback navigation.
    EnterTerminalNormal,
    /// `I` — insert before the first non-blank char of the line.
    InsertLineStart,
    /// `gI` — insert at column 1 (byte column 0), BEFORE all indentation, unlike `I` which stops at the
    /// first non-blank. `{count}gI` repeats the typed text `count` times like the other insert-entries.
    InsertColumnZero,
    /// `A` — append at the end of the line.
    AppendLineEnd,
    /// `o` — open a new line below and insert.
    OpenBelow,
    /// `O` — open a new line above and insert.
    OpenAbove,
    /// `R` — enter Replace mode (overwrite policy).
    EnterReplace,
    /// A printable key typed in Replace mode: overwrite the char under the cursor (or append at EOL).
    ReplaceType(char),
    /// `<BS>` in Replace mode: restore the last overwritten char (or delete the last appended one),
    /// moving the cursor back; at the session start it only moves the cursor left. Shared by Virtual Replace.
    ReplaceBackspace,
    /// `gR` — enter Virtual Replace mode (overwrite policy, tab-aware): like Replace but a `<Tab>` under the
    /// cursor is consumed one virtual column at a time rather than as a whole char.
    EnterVirtualReplace,
    /// A printable key typed in Virtual Replace mode (`gR`): overwrite the char under the cursor, but when
    /// that char is a `<Tab>` insert BEFORE it (shrinking the tab) until its last virtual column, then
    /// replace it; append at end-of-line. `<BS>` uses [`Command::ReplaceBackspace`].
    VirtualReplaceType(char),
    /// `{count}gr{char}` — CLASSIC-Vim virtual-replace of `count` chars with `char`, then return to Normal.
    /// The one-shot form of `gR` (like `r` is to `R`): tab-aware (over a multi-column `<Tab>` it inserts
    /// before the tab, preserving the following text's column), and it APPENDS past end-of-line when `count`
    /// overruns the line (Vim), unlike `r` which is a no-op there. The cursor lands on the last replaced char.
    ///
    /// DIVERGENCE NOTE: Neovim 0.11+ ships `gr`/`grn`/`gra`/`grr` as DEFAULT LSP *keymaps* (rename / code-
    /// action / references), but those are keymaps, not a built-in editor command. ruse installs no such
    /// mapping and targets the classic-Vim built-in, so `gr` here is virtual-replace-one-char.
    VirtualReplaceChar(u32, char),
    // edit
    InsertChar(char),
    /// `i_CTRL-R{reg}` — insert the named register's contents at the caret, staying in Insert (Vim). Reads
    /// the same slots as paste (`"`, `0`–`9`, `-`, `a`–`z`). Recorded in the insert session, so `.` replays
    /// it and re-reads the register at replay time (Vim). Empty/unsupported register inserts nothing.
    InsertRegister(char),
    /// `i_CTRL-R=` — insert the RESULT of evaluating an expression at the caret, staying in Insert (Vim's
    /// expression register; `:help i_CTRL-R`). The frontend collects the expression string via a prompt and
    /// passes it here; the editor evaluates it with [`crate::expr::eval_or_empty`] (a MINIMAL-HONEST
    /// arithmetic/string calculator, NOT full Vimscript) and inserts the formatted result. A malformed or
    /// unsupported expression evaluates to an EMPTY string and inserts nothing (Vim's degrade). The dot-repeat
    /// recorder does not capture the expression string, so `.` does not replay the insert (a documented
    /// divergence).
    InsertEval(String),
    /// A bracketed paste in Insert (`:help paste`, #600): splice `text` at the caret LITERALLY, staying in
    /// the current mode with the caret after it. One edit however large the text, and none of the
    /// insert-time rewrites a typed key gets (no autoindent, no abbreviation, no auto-pair): the frontend
    /// never rewrites this command. Recorded in the insert session, so `.` replays the pasted text. Line
    /// endings are already normalised to `\n` by the frontend. Over a charwise or linewise Visual/Select
    /// selection it REPLACES the selection, as [`Command::ReplaceSelection`] does, and enters Insert.
    InsertPaste(String),
    /// A bracketed paste in Replace mode (`R`): overwrite as many characters of the current line as the
    /// text holds (newlines excluded), then splice the text, as nvim's `vim.paste` does. The caret lands
    /// after the text. A replace-mode `<BS>` does not restore what the paste overwrote.
    ReplacePaste(String),
    /// A bracketed paste under the Emacs profile (Emacs 31.1 `xterm-paste` with the default
    /// `xterm-store-paste-on-kill-ring`): put `text` on the kill ring, then yank it — insert at point,
    /// point after the text, mark at its start.
    EmacsPaste(String),
    /// `i_CTRL-W` — delete the word before the caret, staying in Insert (Vim). Deletes `[word-back, caret)`
    /// on the current line (does not cross the line start). A no-op at column 0.
    InsertDeleteWordBack,
    /// `i_CTRL-U` — delete everything before the caret on the current line, staying in Insert. Deletes back
    /// to the first non-blank; when the caret is already at/before it, deletes the leading indent too (Vim).
    InsertDeleteToLineStart,
    /// `i_CTRL-N` / `i_CTRL-P` — apply one keyword-completion cycle step (F-003): delete `back` CHARACTERS
    /// before the caret (the completion region currently in the buffer — the typed base, or the previously
    /// applied candidate) and insert `text` (the newly selected candidate, or the original typed text when
    /// cycling back), staying in Insert with the caret after the inserted text. The frontend resolves the
    /// candidate set from the buffer (the engine has no buffer); the engine cycles the index and emits this.
    /// Recorded in the insert session, so `.` replays the chain of steps and reproduces the accepted text
    /// literally (matches nvim v0.12.4). `text` is a keyword (no whitespace), or empty when the original
    /// base was empty.
    CompleteWord {
        back: u32,
        text: String,
    },
    /// `i_CTRL-T` — indent the current line by one shiftwidth, staying in Insert; the caret rides with the
    /// text. `i_CTRL-D` dedents by one shiftwidth. Independent of the caret column (Vim).
    InsertIndent,
    /// `i_CTRL-D` — dedent the current line by one shiftwidth (see [`Command::InsertIndent`]).
    InsertDedent,
    /// `i_0_CTRL-D` / `i_^_CTRL-D` — delete ALL indent of the current line, staying in Insert; the caret rides
    /// with the text. (The typed `0`/`^` is removed by a `DeleteBack` the input engine emits first.)
    InsertDeleteIndent,
    /// `i_^_CTRL-D` — [`Command::InsertDeleteIndent`], and the next Insert `<CR>` under `'autoindent'`
    /// gives the new line the deleted indent back (nvim's `old_indent`, #617).
    InsertDeleteIndentRestore,
    /// `i_CTRL-G_j` / `i_CTRL-G_k` — move the caret one line down / up to the column where inserting
    /// started (Vim's `Insstart`: the insert entry, or where typing resumed after a caret jump). A no-op on
    /// the last / first line.
    InsertStartColumnLine {
        down: bool,
    },
    /// `i_CTRL-G_U` then `<Left>`/`<Right>` — move the caret one char within the line WITHOUT breaking the
    /// undo sequence (and, in the input engine, without ending the `.` record).
    InsertCaretStep {
        right: bool,
    },
    /// `<Tab>` in Insert mode: insert whitespace at the caret to reach the next tabstop. With `expandtab`
    /// (space style) this is `tab_width - (vcol % tab_width)` spaces; otherwise a single `\t`. Distinct from
    /// `InsertIndent` (`i_CTRL-T`), which shifts the whole line from its start regardless of caret column.
    InsertTab,
    InsertNewline,
    /// `o`/`O`/`<CR>` seeded with a tree-suggested indent (F-015 Phase 2): open a line (per `kind`) whose
    /// leading whitespace is `level × shiftwidth`, leaving the cursor after it in Insert. The frontend owns
    /// the syntax tree, so it computes `level` and rewrites the plain open commands into this (the core is
    /// tree-sitter-free); the trace replays the concrete indent exactly. `level: 0` is a plain open.
    OpenLineIndent {
        kind: OpenKind,
        level: usize,
    },
    /// `}`/`)`/`]` typed as the SOLE non-blank on a line (F-015 Phase 3a, smartindent-like): realign the
    /// line's leading whitespace to the matching opener's line indent, then insert the closer. Deterministic
    /// bytes bracket-match (no string/comment awareness, like the bracket-depth `=` fallback); if the line
    /// already has non-blank content before the cursor, or no matching opener is found, it is a plain insert.
    /// The frontend rewrites `InsertChar(closer)` into this for tree-backed (code) buffers only.
    InsertCloser {
        ch: char,
    },
    DeleteBack,
    /// `{count}x` — delete `count` chars from the cursor, clamped at end-of-line (Vim).
    DeleteUnder(u32),
    /// Emacs `delete-char` / `C-d` — delete `count` chars forward from point, WITHOUT writing to the kill
    /// ring (D-026: Emacs discards a `delete-char`, unlike Vim `x`). Clamped at buffer end and crosses
    /// newlines (Emacs is non-modal, so end-of-line is not a boundary), never touching the register.
    DeleteForward(u32),
    /// `{count}r{char}` — replace `count` chars with `char`; a no-op if fewer than `count` remain (Vim).
    ReplaceChar(u32, char),
    /// Visual `r{char}` — replace EVERY non-newline character in the selection with `char`, preserving line
    /// breaks and the selection's length; cursor to the start, back to Normal.
    ReplaceSelectionChar(char),
    /// `{count}~` — toggle the case of `count` chars, clamped at EOL, then move past the last (Vim).
    ToggleCase(u32),
    /// `gu`/`gU`/`g~` {motion} — recase the operator span with `case` (lower / upper / toggle), leaving
    /// the cursor at the start of the range. Operator-over-motion, like `d`/`c`/`y`.
    CaseMotion {
        count: u32,
        motion: Motion,
        case: WordCase,
    },
    /// Visual `u`/`U`/`~` — recase the whole selection (lower / upper / toggle), leaving the cursor at the
    /// selection start and returning to Normal. A no-op outside a selection.
    CaseSelection(WordCase),
    /// `[count]J` — join lines on a single space. `count` joins `count-1` seams (Vim: `J`/`2J` = one join,
    /// `3J` = two), leaving the cursor on the last join. The space is suppressed before `)`, after trailing
    /// whitespace, or on an empty line.
    JoinLines(u32),
    /// `[count]gJ` — like [`Command::JoinLines`] but WITHOUT inserting a space or stripping the next line's
    /// leading whitespace: only the newlines are removed (Vim `gJ`).
    JoinLinesNoSpace(u32),
    /// `CTRL-A` / `CTRL-X` — add the signed `i64` to the decimal number at or after the cursor on the
    /// current line (`CTRL-A` = +count, `CTRL-X` = −count). No-op if the line has no number after the
    /// cursor. Decimal only; the cursor lands on the last digit of the result (Vim).
    IncrementNumber(i64),
    /// Visual `CTRL-A`/`CTRL-X` (`delta` = ±count) over the selection: add `delta` to the first number on
    /// EACH selected line. `sequential` (the `g CTRL-A`/`g CTRL-X` form) instead adds `delta`, `2·delta`,
    /// `3·delta`… to successive numbered lines — turning a column of equal numbers into an ascending run.
    /// Lines without a number are skipped and do not advance the sequence. Returns to Normal, caret on the
    /// FIRST SELECTED line at the selection's left-edge column (col 0 linewise / block-left blockwise /
    /// selection-start charwise — independent of which line changed). Charwise, linewise, and blockwise
    /// selections are all supported (blockwise targets the block's left column, not the line's first number).
    IncrementSelection {
        delta: i64,
        sequential: bool,
    },
    /// `` `. `` — jump the cursor to the position of the most recent change (Vim's automatic `.` mark).
    /// A no-op before the first edit. Operator-pending (`` d`. ``) and named marks are deferred.
    GotoLastChange,
    /// `'.` — jump LINEWISE to the first non-blank of the last change's line (Vim). No-op before any edit.
    GotoLastChangeLine,
    /// `` `[ `` — jump to the FIRST char of the previously changed or yanked text (Vim's `[` mark). The mark
    /// is set automatically after every change (insert/delete/replace/put) and every yank. No-op if unset.
    GotoChangeMarkStart,
    /// `` `] `` — jump to the LAST char of the previously changed or yanked text (Vim's `]` mark). For a
    /// delete/replace/put/yank this is the last char inclusive; for an Insert session it is the insert
    /// end-caret (Neovim), clamped onto the line's last char when jumping. No-op if unset.
    GotoChangeMarkEnd,
    /// `'[` — jump LINEWISE to the first non-blank of the `[` mark's line (first affected line). No-op if unset.
    GotoChangeMarkStartLine,
    /// `']` — jump LINEWISE to the first non-blank of the `]` mark's line (last affected line). No-op if unset.
    GotoChangeMarkEndLine,
    /// `` `< `` — jump to the FIRST char (start) of the LAST visual/select selection (Vim's `<` mark).
    /// Charwise: the byte-min of the selection's two ends; linewise: column 0 of the first selected line;
    /// blockwise: the top corner. Backed by the same depth-1 store as `gv` ([`Command::ReselectVisual`]),
    /// set on every Visual exit (Esc AND operator-completion). No-op before any visual selection has ended.
    GotoVisualMarkStart,
    /// `` `> `` — jump to the LAST char (end) of the last visual/select selection (Vim's `>` mark). Charwise:
    /// the byte-max of the two ends; linewise: the last char of the last selected line; blockwise: the bottom
    /// corner. Same store as `` `< ``. No-op before any visual selection has ended.
    GotoVisualMarkEnd,
    /// `'<` — jump LINEWISE to the first non-blank of the FIRST selected line (Vim's `<` mark, line form).
    /// No-op before any visual selection has ended.
    GotoVisualMarkStartLine,
    /// `'>` — jump LINEWISE to the first non-blank of the LAST selected line (Vim's `>` mark, line form).
    /// No-op before any visual selection has ended.
    GotoVisualMarkEndLine,
    /// `` `` `` — jump to the position before the latest jump (Vim's automatic `` ` ``/`'` context mark).
    /// Reads the newest jumplist entry; being itself a jump, it records where it left, so repeating toggles
    /// between the two positions. A no-op before any jump has happened.
    GotoContextMark,
    /// `''` — jump LINEWISE to the first non-blank of the context mark's line (Vim). No-op before any jump.
    GotoContextMarkLine,
    /// `g;` — jump to the next OLDER position in the change list (Vim). A no-op at the oldest change.
    GotoOlderChange,
    /// `g,` — jump to the next NEWER position in the change list (Vim). A no-op at the newest change.
    GotoNewerChange,
    /// `CTRL-O` — jump to the next OLDER position in the jumplist (Vim). A no-op at the oldest jump.
    GotoOlderJump,
    /// `CTRL-I` / `Tab` — jump to the next NEWER position in the jumplist (Vim). A no-op at the newest.
    GotoNewerJump,
    /// `gi` — resume Insert at the last-insert position (Vim's `` `^ ``). Enters Insert at buffer start
    /// before any Insert session has ended.
    InsertAtLastInsert,
    /// `ga` / `:ascii` / `:as` — print the numeric value of the character under the cursor to the status
    /// line (no buffer mutation). Frontend-resolved (the engine has no buffer): the frontend reads the
    /// focused buffer's bytes + cursor and formats via [`crate::info::ascii_info`].
    AsciiInfo,
    /// `g8` — print the UTF-8 bytes of the character under the cursor in hex, composing characters after a
    /// `+` ([`crate::info::utf8_info`]). Frontend-resolved like [`Command::AsciiInfo`].
    Utf8Info,
    /// `CTRL-G` (Normal) — print file info (name, modified flag, line count, cursor percentage) to the
    /// status line. Frontend-resolved via [`crate::info::file_info`]; no buffer mutation.
    FileInfo,
    /// `g CTRL-G` — print cursor position and buffer counts (column/line/word/char/byte) to the status
    /// line. Frontend-resolved via [`crate::info::cursor_pos_info`]; no buffer mutation.
    CursorInfo,
    /// `m{a-z}` — set the named mark `char` at the cursor (Vim). Per-buffer; the cursor does not move.
    SetNamedMark(char),
    /// `` `{a-z} `` — jump the cursor to named mark `char` (Vim). A no-op if that mark is unset.
    GotoNamedMark(char),
    /// `'{a-z}` — jump LINEWISE to the first non-blank of named mark `char`'s line (Vim). No-op if unset.
    GotoNamedMarkLine(char),
    /// `` d`{a-z} `` / `d'{a-z}` (and `c`/`y`/case/shift/reindent) — an operator whose motion is a named
    /// mark. `linewise` is the `'` form (whole lines from the cursor's line to the mark's line); `` ` `` is
    /// exclusive charwise from the cursor to the mark. A no-op if the mark is unset. The mark lives in the
    /// View, so — unlike a pure motion — this resolves in the planner, not `motion::target`. `op` may be
    /// delete/change/yank, a case op (`` g~`a ``), a shift (`` >`a ``) or reindent (`` =`a ``); shift and
    /// reindent are always linewise and ignore `linewise` (Vim: they operate on whole lines).
    OpToMark {
        op: MarkOp,
        name: char,
        linewise: bool,
    },
    /// `CTRL-G u` in Insert — break the undo sequence: the NEXT edit starts a fresh undo group, so a
    /// later `u` stops here instead of undoing the whole insert session. A nop that only clears the
    /// edit-continuation state (Vim `i_CTRL-G_u`). Undo-of-a-session is not observable via the parity
    /// oracle (its `set_lines` is not an undo boundary), so this is verified by a core unit test.
    BreakUndo,
    /// `{count}>>` — shift `count` lines one indent level to the right (Vim `>>`). Linewise: the count
    /// is a LINE count, not a motion. The indent unit comes from the editor's indent config.
    ShiftRight(u32),
    /// `{count}<<` — shift `count` lines one indent level to the left (Vim `<<`). Symmetric inverse of
    /// [`Command::ShiftRight`]: removes up to one indent level of leading whitespace, never past column 0.
    ShiftLeft(u32),
    /// `>`/`<` {motion} — shift the motion's LINES one indent level (always linewise; `left` picks the
    /// direction). The planner resolves the motion span to a line range, then indents those lines.
    ShiftMotion {
        left: bool,
        count: u32,
        motion: Motion,
    },
    /// `=` {motion} — reindent the motion's LINES to their bracket-depth level (net unclosed `([{` ×
    /// shiftwidth; a closer-first line dedents one). Structural, language-agnostic (no string/comment
    /// awareness); the doubled `==` uses `motion: Line`.
    Reindent {
        count: u32,
        motion: Motion,
    },
    /// `gc` {motion} (and `gcc`, `motion: Line`) — comment the motion's LINES out with `'commentstring'`,
    /// or uncomment them when every non-blank one already is (nvim's `vim._comment`, #618). An empty
    /// `'commentstring'` does nothing.
    Comment {
        count: u32,
        motion: Motion,
    },
    /// `gq` / `gw` {motion} — reflow the motion's LINES to `'textwidth'` (or 79 when tw=0): each blank-line-
    /// separated paragraph is re-wrapped, preserving its first line's indent. `gw` (`keep_cursor`) restores
    /// the cursor to where it started; `gq` leaves it on the last reformatted line. Operator-over-motion.
    Format {
        count: u32,
        motion: Motion,
        keep_cursor: bool,
    },
    /// Set the indent level of each line in `[first_line, last_line]` to `levels[i] × shiftwidth`
    /// (blank lines stay empty), as one undo group. The frontend emits this for the tree-aware `=` — it
    /// computes the levels from the syntax tree, so the levels are concrete and the trace replays exactly
    /// (the core is tree-sitter-free). `Reindent` above is the non-tree bracket-depth fallback.
    SetIndents {
        first_line: usize,
        last_line: usize,
        levels: Vec<usize>,
    },
    /// Set the indent of each line in `[first_line, last_line]` to `cols[i]` columns, or keep it for
    /// `None` (an indent expression's `-1`); blank lines stay empty. The frontend emits it for an indenter
    /// that answers in columns (Python's `python#GetIndent`: a continuation line aligns with its open
    /// bracket, #1205): for `=` in Normal mode, as `SetIndents` does, and in Insert mode for the line under
    /// the caret, re-indented by a typed key ('indentkeys', #1206), which keeps the caret on its text.
    SetIndentCols {
        first_line: usize,
        last_line: usize,
        cols: Vec<Option<usize>>,
    },
    /// `i_CTRL-F` (`!^F` in 'indentkeys'): re-indent the line under the caret. The frontend resolves it
    /// to a [`Command::SetIndentCols`] (or [`Command::SetIndents`]) from the buffer's indenter; without one
    /// it does nothing, as in nvim without an indent expression.
    InsertReindent,
    // editing grammar: count + motion / operator (Phase D)
    Move(u32, Motion),
    Delete(u32, Motion),
    Change(u32, Motion),
    /// `{op}{v|V}{motion}` — an operator over a motion whose wise is FORCED (Vim `o_v`/`o_V`): `dvj`
    /// deletes charwise where `dj` is linewise; `dVe` deletes whole lines where `de` is charwise. Kept
    /// distinct from the plain operator commands so the common path stays a bare tuple; `count` multiplies
    /// the motion as usual.
    OpForced {
        op: OpKind,
        count: u32,
        motion: Motion,
        wise: ForcedWise,
    },
    // registers (v0 unnamed slot): yank a motion range; paste after (`p`) or before (`P`) the cursor
    Yank(u32, Motion),
    /// `{count}p` / `{count}P` — paste the unnamed register `count` times, after (`p`) or before (`P`)
    /// the cursor. `count` is the repetition count (Vim `2p` pastes twice); it defaults to 1.
    Paste {
        after: bool,
        count: u32,
        /// `gp`/`gP` (Vim): leave the cursor JUST AFTER the pasted text instead of on/before it.
        move_after: bool,
    },
    /// Visual `p` / `P` — replace the current selection with the register's contents. `swap` (Vim `p`) puts
    /// the deleted selection into the unnamed register; `P` (`swap: false`) preserves the register so it can
    /// be pasted over successive selections. `count` puts the register that many times (`v2p`, #1093). A
    /// no-op outside a selection.
    PasteSelection {
        swap: bool,
        count: u32,
    },
    /// `]p` / `[p` (and `]P`/`[P`) — paste like `p`/`P` but ADJUST the indent of a linewise register to the
    /// current line: the first pasted line takes the current line's indent, the rest shift by the same
    /// column delta (Vim `:help ]p`). A charwise/blockwise register pastes unchanged (Vim `]p` = `p` there).
    PasteIndent {
        after: bool,
        count: u32,
    },
    /// `zp` / `zP` — paste like `p`/`P`, but a blockwise register's short rows get no trailing spaces
    /// (Vim `:help zp`). Any other register pastes exactly as `p`/`P` does.
    PasteBlockInner {
        after: bool,
        count: u32,
    },
    /// `"x` — select the register `x` (`a`–`z`, or `A`–`Z` to append) for the FOLLOWING yank/delete/
    /// change/paste. `None` selects the unnamed register (the default). Emitted by the input engine on the
    /// register name key; the editor holds it as a one-shot pending register the next such command reads,
    /// then clears (D-026's named-slot expansion). The numbered ring `"1`-`"9`, small-delete `"-`,
    /// black-hole `"_`, and clipboard `"+`/`"*` ARE modelled (see `register.rs`); the expression register
    /// `"=` is special and set via [`Command::SetExprRegister`] instead (its contents are computed).
    SetRegister(Option<char>),
    /// `"=<expr><CR>` — set the expression register `"=` for the FOLLOWING paste (`:help quote=`). The
    /// frontend collects the expression string via a prompt and passes it here; the editor evaluates it with
    /// [`crate::expr::eval_or_empty`] (a MINIMAL-HONEST arithmetic/string calculator, NOT full Vimscript),
    /// stores the formatted result in the `"=` slot, and arms `=` as the pending register so the next `p`/`P`
    /// pastes it. A malformed/unsupported expression stores an EMPTY result, so that paste does nothing
    /// (Vim's degrade). Like `SetRegister` it makes no edit and is not itself a dot-repeatable change.
    SetExprRegister(String),
    // visual mode: enter a selection (charwise `v` / linewise `V` / blockwise `CTRL-V`); operate on it
    EnterVisual {
        kind: SelectKind,
    },
    /// `CTRL-G` from Visual — enter Select over the SAME selection (Visual↔Select toggle). Select
    /// shares Visual's anchor/shape and differs only in its unmatched-key policy (contracts/vim-style.yaml).
    EnterSelect {
        kind: SelectKind,
    },
    DeleteSelection,
    YankSelection,
    ChangeSelection,
    /// `gv` — re-select the LAST visual/select selection (its anchor, active end, and charwise/linewise
    /// kind), captured when that selection was last left. A no-op if there is no prior selection. This is
    /// the depth-1 degenerate of D-027's `` `< ``/`` `> `` selection history (one remembered `Selection`).
    ReselectVisual,
    /// Visual/Select `o` — swap the selection's two ends: the cursor jumps to the anchor and the anchor
    /// becomes the old cursor position. The SAME text stays selected, but subsequent motions now extend
    /// the OTHER end. Involutive (`oo` restores the original ends); a clean no-op outside a selection.
    SwapSelectionEnds,
    /// Select's `open/replace-selection` policy: a printable key deletes the selection, inserts the char,
    /// and enters Insert. The one behaviour that distinguishes Select from Visual over identical state.
    ReplaceSelection(char),
    /// A line-oriented (or block-to-end-of-line) Visual operator over the live selection: `D`/`X`/`Y`/
    /// `C`/`S`/`R`, `>`/`<`, `J`/`gJ`, `=`, `gq`, and block `O`. See [`VisualOp`].
    VisualOp(VisualOp),
    /// Blockwise Visual `I`/`A`/`c`: enter Insert on the block's top row, arming a replicate session — on
    /// `<Esc>` the text typed on the top row is inserted at the same column on every other row of the block.
    BlockInsert(BlockInsertKind),
    // search (literal substring for v0; the pattern is carried so traces replay deterministically)
    SearchNext(String),
    SearchPrev(String),
    /// `*`/`#` (and `g*`/`g#`) — search for the keyword under the cursor, forward (`*`) or backward (`#`).
    /// `whole_word` (`*`/`#`) wraps the pattern in `\<…\>`; `g*`/`g#` match anywhere. The planner resolves
    /// the keyword under (or after, on the line) the cursor and searches from the keyword's START, as Vim
    /// does (`#` mid-word skips the word itself). Like `/pat` it is a motion: `count` steps that many
    /// matches and `op` operates over the exclusive span from the cursor (`d*`, `c#`, `gUg*` — #1071). The
    /// frontend reads the same pattern ([`crate::Workspace::word_search_pattern`]) to record it as the last
    /// search and for hlsearch.
    SearchWordUnder {
        forward: bool,
        whole_word: bool,
        op: SearchOp,
        count: u32,
    },
    /// `g&` — repeat the last `:s` over the WHOLE FILE with its flags (Vim). The core has no substitute
    /// history, so the FRONTEND resolves this against its last-substitute state (like [`SearchWordUnder`]);
    /// the planner treats it as a no-op.
    RepeatSubstituteGlobal,
    /// `&` — repeat the last `:s` on the CURRENT LINE, KEEPING its flags: nvim's default mapping is
    /// `nnoremap & :&&<CR>` (`:help &-default`), unlike Vim's flag-dropping `&`. Contrast with
    /// [`RepeatSubstituteGlobal`] (`g&`), which runs over the whole file. Like `g&`, the core has no
    /// substitute history, so the FRONTEND resolves this against its last-substitute state (the typed key
    /// with its count, this command without one); the planner treats it as a no-op.
    RepeatSubstituteLine,
    /// `gd` / `gD` — go to the (local / global) declaration of the keyword under the cursor using Vim's pure
    /// TEXT heuristic (NOT LSP; ruse has a separate LSP goto). The frontend reads the word from the buffer
    /// (the input engine has no buffer) and rewrites this to a concrete [`Command::GotoFirstMatch`] with the
    /// whole-word pattern, so traces record the deterministic jump. `global` distinguishes `gd` (`false`)
    /// from `gD` (`true`); both currently resolve identically — the first WHOLE-WORD match from the top of
    /// the file — which is exactly what nvim v0.12.4 does for both (verified). The `gd` local-scope
    /// refinement (limit to the enclosing `{`-block) is a documented follow-up and reads this flag.
    GotoDeclaration {
        global: bool,
    },
    /// The concrete jump `gd`/`gD` resolve to (via [`Command::GotoDeclaration`]): move to the FIRST match of
    /// the whole-word pattern scanning from the TOP of the file (offset 0), landing on the match's first
    /// char. No match keeps the cursor (Vim rings the bell). A JUMP — records the leaving position so
    /// `CTRL-O` returns. The pattern is carried so traces replay deterministically (like [`SearchNext`]).
    GotoFirstMatch(String),
    /// `[i` / `]i` / `[I` / `]I` — DISPLAY the line(s) in the CURRENT buffer containing the keyword under
    /// the cursor (`:help [i`). `above` = the `[` forms (scan from the TOP of the file); `!above` = the
    /// `]` forms (below the cursor line). `list` = the uppercase `[I`/`]I` list-ALL forms; `!list` = the
    /// lowercase single-line echo. `count` selects the N-th match for the single forms (ignored by the
    /// list forms). Purely a display command (no buffer mutation): the input engine has no buffer, so the
    /// FRONTEND resolves the keyword and scans the lines (like [`SearchWordUnder`]/[`GotoDeclaration`])
    /// and never plans it. The `#include`-following forms (`:checkpath`, searching included files) are
    /// OUT (cross-file); ruse has no `iskeyword`, so word boundaries follow Vim's character classes with
    /// the default `iskeyword` (`crate::charclass`, #745). Formats verified against nvim v0.12.4.
    ShowKeywordLines {
        above: bool,
        list: bool,
        count: u32,
    },
    /// `{count}/{pattern}<CR>` (forward) or `{count}?{pattern}<CR>` (backward) as a MOTION: bare it moves
    /// to the `count`-th match in `backward`'s direction; under an operator (`op`) it folds into a charwise-
    /// EXCLUSIVE edit over `[cursor, match)` forward, or `[match, cursor)` backward (`d/pat`, `c?pat`,
    /// `y?pat`). The pattern is literal (v0; C-REGEX later) and carried so traces replay deterministically.
    /// `n`/`N` repeat RELATIVE to the last search's direction via [`Command::SearchNext`]/
    /// [`Command::SearchPrev`] (the frontend picks which based on the stored direction).
    Search {
        op: SearchOp,
        count: u32,
        pattern: String,
        /// `?` search (match strictly before the cursor, wrapping to end of buffer); `false` for `/`.
        backward: bool,
        /// The `{offset}` after the closing delimiter (`/pat/e`, `?pat?s+2`, `/pat/+1`). Relocates the
        /// landing and, under an operator, sets the motion's wiseness (`:help search-offset`). `n`/`N`
        /// reapply this offset. See [`SearchOffset`].
        offset: SearchOffset,
    },
    /// `gn` / `gN` — the search-match text object (Vim `:help gn`). Unlike [`Command::Search`] (which spans
    /// `[cursor, match)`), this operates on the WHOLE match: the one under the cursor, or — if the cursor is
    /// not on a match — the next (`gn`, `backward: false`) or previous (`gN`, `backward: true`) match.
    /// `count` advances that many matches. `op` = [`SearchOp::Move`] is the bare form: enter Visual with the
    /// match selected (so `gn` then an operator works); `Delete`/`Change`/`Yank` operate on the match
    /// directly (`dgn`/`cgn`/`ygn`). Because the pattern is baked in, `.` replays `cgn` — the idiom that
    /// makes gn powerful (change one match, `n.` through the rest).
    SearchObject {
        op: SearchOp,
        count: u32,
        pattern: String,
        backward: bool,
    },
    // history / file / control
    Undo,
    Redo,
    /// `g-` — step to the chronologically older text state, across branches (Vim undo-time travel).
    UndoOlder,
    /// `g+` — step to the chronologically newer text state, across branches.
    UndoNewer,
    /// `U` — undo all the latest changes on the last changed line; itself undoable, and a second `U` redoes.
    LineUndo,
    Save,
    Quit,
    // Emacs region (D-027 depth-1: a single per-buffer mark, the degenerate one-caret `Ring<Selection>`).
    /// `C-SPC` (set-mark-command) — set the mark at point. The region is the span between point and mark.
    SetMark,
    /// `C-w` (kill-region) — delete the region `[min(point,mark), max)` charwise into the unnamed register
    /// (the kill ring), leaving point and mark together at its start. A no-op when no mark is set.
    KillRegion,
    /// `M-w` (kill-ring-save) — copy the region into the register without deleting; point and mark are
    /// unchanged. A no-op when no mark is set.
    CopyRegion,
    /// `C-x C-x` (exchange-point-and-mark) — swap point and mark. A no-op when no mark is set.
    ExchangePointMark,
    /// `C-y` (yank) — paste the register before point AND set the mark at the insertion start, leaving point
    /// after the pasted text (D-051). Emacs yank differs from Vim `p`/`P` (which never touch the mark); the
    /// paste geometry itself is shared with [`Command::Paste`] `{after:false}` (gravity-aware cursor, D-050).
    EmacsYank {
        count: u32,
    },
    /// `M-<` / `M->` (beginning-of-buffer / end-of-buffer) — move point to the ABSOLUTE buffer start (`0`)
    /// or end (buffer length), and PUSH the mark at the old point (D-051). Distinct from Vim `gg`/`G`
    /// (`Move(GotoLine/LastLine)`), which land on a line's first non-blank and never set this mark.
    EmacsBufferEdge {
        start: bool,
    },
    /// `C-k` (kill-line) — kill from point to end of line into the register (the kill ring). Distinct from
    /// Vim `D` / `Delete(1, LineEnd)`: when point is already AT end of line, Emacs kills the terminating
    /// newline instead (joining the next line up), and at end-of-buffer it is inert. The count-less default
    /// (`kill-whole-line` nil, no prefix arg) — the binding ignores the prefix count (D-051 / RFC-0016:
    /// an Emacs op diverging from its Vim lookalike in more than caret gravity is its own command).
    EmacsKillLine,
    /// `C-t` (transpose-chars) — swap the character before point with the character at point, then advance
    /// point past the pair. At end of line it transposes the two characters that end the line (Emacs steps
    /// back one first). Inert when there is no pair to transpose (buffer/line start with fewer than two
    /// chars on the line). Emacs-only, no Vim lookalike (D-051); it does not touch the kill ring.
    EmacsTransposeChars,
    /// `M-t` (transpose-words) — swap the word before/around point with the following word, preserving the
    /// separator between them, and leave point past the second (now-moved) word. Emacs-only (D-051); no
    /// kill-ring write. Inert when there is not a pair of words to transpose.
    EmacsTransposeWords,
    /// `M-d` / `kill-word` (Emacs kill-word) — kill the `EmacsWordFwd` span (the word only, not Vim `dw`'s
    /// trailing space) into the register, `count` words. Distinct from Vim `Delete(count, EmacsWordFwd)`
    /// ONLY in that it ACCUMULATES onto the current kill-ring entry when it follows another kill — but that
    /// alone earns its own command (D-051), so Vim deletes never accumulate.
    EmacsKillWord {
        count: u32,
    },
    /// `M-DEL` (backward-kill-word) — kill `count` words BACKWARD (the `WordBack` span) into the register.
    /// Like the other Emacs kills it accumulates, but as a BACKWARD kill it PREPENDS onto the current
    /// kill-ring entry (Emacs `kill-append` with `before=t`). Distinct from Vim `db` (which never
    /// accumulates) — D-051.
    EmacsBackwardKillWord {
        count: u32,
    },
    /// `M-u` / `M-l` / `M-c` (upcase-word / downcase-word / capitalize-word) — recase the word from point to
    /// the end of the next word (the `forward-word` span) and leave point at that end. Emacs-only (D-051);
    /// the three keys differ only in the case operation, so they share one command carrying [`WordCase`].
    EmacsCaseWord {
        case: WordCase,
    },
    /// `M-SPC` (just-one-space, `keep_one`) / `M-\` (delete-horizontal-space) — delete all spaces and tabs
    /// around point (never crossing a newline). `just-one-space` leaves exactly ONE space and rests point
    /// after it (inserting a space when there was none); `delete-horizontal-space` leaves none. Emacs-only
    /// (D-051); no kill-ring write.
    EmacsHorizontalSpace {
        keep_one: bool,
    },
    /// `C-o` (open-line) — insert a newline at point but leave point BEFORE it (opening a blank line below
    /// the cursor without moving onto it). Distinct from Vim `o` (which opens below AND enters Insert on the
    /// new line) — D-051. No kill-ring write.
    EmacsOpenLine,
    /// `M-@` (mark-word) — set the mark at the end of the next word (the `forward-word` target) without
    /// moving point, activating the region point→word-end. Emacs-only (D-051); no kill-ring write.
    EmacsMarkWord,
    /// `C-S-DEL` (kill-whole-line) — kill the ENTIRE current line including its trailing newline, regardless
    /// of point column, leaving point at the start of the following line. An accumulating kill (D-051 /
    /// D-026). Distinct from Vim `dd` (linewise register, cursor to first-non-blank) — this is charwise into
    /// the kill ring.
    EmacsKillWholeLine,
    /// `C-x C-u` / `C-x C-l` (upcase-region / downcase-region; `capitalize-region` too) — recase the active
    /// region `[min(point,mark), max)` with the given [`WordCase`], leaving point and mark unchanged.
    /// Emacs-only (D-051); no kill-ring write. Inert when no mark is set.
    EmacsCaseRegion {
        case: WordCase,
    },
    /// `M-^` (delete-indentation / join-line) — join the current line to the PREVIOUS one: delete the
    /// preceding newline plus the previous line's trailing whitespace and this line's leading indentation,
    /// then fix up whitespace to a single space (or none when the join lands at beginning-of-line). Point
    /// rests at the join. Distinct from Vim `J` (which joins the NEXT line onto the current one) — D-051.
    /// No kill-ring write.
    EmacsDeleteIndentation,
    /// A resolved Ex edit, or the frontend state one depends on (RFC-0020, D-061): `:s`, `:g`, `:d`, `:m`,
    /// `:sort`, `:r` with its bytes, `:normal`'s group, … with every line number and pattern already
    /// resolved, so a trace replays it without an Ex parser, cursor context or IO.
    Ex(crate::editor::ExCmd),
}

/// Which case operation [`Command::EmacsCaseWord`] applies over the word span.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WordCase {
    /// `M-u` upcase-word — every letter to upper case.
    Upcase,
    /// `M-l` downcase-word — every letter to lower case.
    Downcase,
    /// `M-c` capitalize-word — first letter of each word upper, the rest lower.
    Capitalize,
    /// Vim `g~` — flip each letter's case (upper↔lower). Not produced by the Emacs case commands.
    Toggle,
    /// Vim `g?` — ROT13 each ASCII letter (Caesar shift by 13). Not produced by the Emacs case commands.
    Rot13,
}

mod line_form;
pub(crate) use line_form::hex_lower;
pub use line_form::CommandParseError;

#[cfg(test)]
mod tests;
