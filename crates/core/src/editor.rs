//! The editor state and the pure **plan / commit** command pipeline (RFC-0012).
//!
//! [`plan`] is a *pure decision*: `(&EditorState, &Command) -> Plan`, no mutation, no IO. [`commit`] applies
//! a `Plan` and returns the [`Effect`]s the frontend must perform. Because the core never does IO, replaying
//! the same commands on the same initial document is deterministic (see [`crate::trace`]). This is the split
//! that captures most of a Haskell rewrite's benefit in Rust — enforced by an empty dependency set.

use crate::command::{Command, SearchOp};
use crate::document::{Document, DocumentId};
use crate::edit::{Edit, EditList};
use crate::effect::Effect;
use crate::motion::{
    self, at_col, col_of, line_end, line_start, next_boundary, prev_boundary, snap, Motion,
};
use crate::register::{Register, RegisterStore};
use crate::transaction::{GroupHint, Transaction, TransactionOrigin};

/// The editor mode. `Visual { line }` and `Select { line }` are selection modes: charwise (`v`) or
/// linewise (`V`). Blockwise (`Ctrl-V`) is deferred. A selection is the pair `(anchor, cursor)`; a bare
/// caret is the degenerate collapsed selection, so extending to multi-selection later needs no type
/// rewrite (D-027 trajectory).
///
/// `Select` shares the SAME selection state as `Visual` (they toggle with `CTRL-G`) and differs only in
/// its unmatched-key policy: in Select a printable key deletes the selection and enters Insert
/// (`open/replace-selection`), where Visual ignores it. This is the census's own framing — two modes over
/// identical state distinguished by the one dimension a transition table could not record
/// (contracts/vim-style.yaml, `namespaces.select`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Normal,
    Insert,
    Visual { line: bool },
    Select { line: bool },
}

/// The indentation unit a shift operator (`>>`/`<<`) applies, derived from the two editor config keys
/// `editor.indent_style` (space|tab) and `editor.tab_width`. Modelled here rather than as ad-hoc constants
/// so the shift commands read the same knobs a future runtime config loader will set — no NEW schema key is
/// introduced (spec/config-schema.yaml already owns both). Defaults match the schema: spaces, width 4.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IndentStyle {
    /// `editor.indent_style = space`: one indent level is `editor.tab_width` spaces.
    Space,
    /// `editor.indent_style = tab`: one indent level is a single `\t`.
    Tab,
}

impl Mode {
    /// Whether this mode carries a live selection (Visual or Select) — the anchor is `Some` exactly then.
    /// `line` is the selection's linewise flag when it has one.
    #[must_use]
    fn selection(self) -> Option<bool> {
        match self {
            Mode::Visual { line } | Mode::Select { line } => Some(line),
            Mode::Normal | Mode::Insert => None,
        }
    }
}

/// Editor state over one document: the buffer, a byte cursor (always on a char boundary), and the mode.
/// View-local state lives here for the headless spine; a real multi-view split comes later (INV-DOC-VIEW).
pub struct EditorState {
    pub doc: Document,
    cursor: usize,
    mode: Mode,
    /// Whether the previous command edited text — drives undo grouping: an edit right after a non-edit
    /// (a motion or a mode change) starts a new undo group (persistence §6); consecutive edits coalesce.
    last_was_edit: bool,
    /// The register store: the unnamed slot plus named `a`–`z` (`A`–`Z` append). Text yanked (`y`) or
    /// deleted (`d`/`c`/`x`) lands here for a later paste (`p`/`P`); an edit into `"x` mirrors the unnamed
    /// slot too. The numbered delete-ring / `"0` yank register stay deferred (D-026; see `register.rs`).
    registers: RegisterStore,
    /// The one-shot register selected by `"x` — the slot the NEXT yank/delete/change/paste targets, then
    /// cleared. `None` = the unnamed register (the default). Set by [`Command::SetRegister`] and consumed by
    /// the following register command in [`commit`]; any other committed command also clears it (a stray
    /// `"x` followed by a plain motion forgets the selection, as in Vim).
    pending_register: Option<char>,
    /// The Visual-mode selection anchor (the fixed end; the cursor is the moving end). `Some` exactly while
    /// in `Mode::Visual`. The full anchor-store-backed `Selection` set is deferred (D-027).
    anchor: Option<usize>,
    /// The LAST selection left behind, as `(anchor, active_end, linewise)` — captured whenever a Visual/
    /// Select mode is exited, restored by `gv` ([`Command::ReselectVisual`]). This is the depth-1
    /// degenerate of D-027's `` `< ``/`` `> `` selection history: one remembered selection, stored in the
    /// same raw-offset representation as the live `anchor` (both migrate to the anchor store together).
    last_visual: Option<(usize, usize, bool)>,
    /// `editor.tab_width` — one indent level's width in columns/spaces. Schema default 4.
    tab_width: usize,
    /// `editor.indent_style` — whether an indent level is spaces or a tab. Schema default `space`.
    indent_style: IndentStyle,
}

enum Action {
    Txn {
        edits: EditList,
        hint: GroupHint,
    },
    Undo,
    Redo,
    Nop,
    /// Install the one-shot pending register (`"x`). Distinct from `Nop` so [`commit`] knows NOT to clear
    /// the pending register it just set — every other action clears it once its command has consumed it.
    SetPending(Option<char>),
}

/// How a committed command should route its captured text into the register store. A `Yank` additionally
/// seeds the yank register `"0` (when unregistered); an `Edit` (delete/change/`x`) never touches `"0`, so
/// `"0` survives intervening deletes (Vim `:help quote0`).
enum RegWrite {
    Edit(Register),
    Yank(Register),
}

/// The pure result of [`plan`]: what a command would do, before any mutation.
pub struct Plan {
    action: Action,
    cursor: usize,
    mode: Mode,
    is_edit: bool,
    effects: Vec<Effect>,
    /// Text to store into a register on commit — an `Edit` capture (delete/change) or a `Yank` (which also
    /// seeds `"0`). `None` when the command writes no register.
    set_register: Option<RegWrite>,
    /// A new selection anchor to install on commit — set only when a text object issued in a selection mode
    /// (`viw`/`vi(`) must move BOTH ends, unlike a bare motion that only moves the cursor. `None` leaves the
    /// anchor to the mode-transition logic in [`commit`].
    set_anchor: Option<usize>,
}

/// Whether a motion is a text object (a range around the cursor), as opposed to a bare cursor movement. In a
/// selection mode these set both ends of the selection; everywhere else they are operator operands.
fn is_text_object(m: Motion) -> bool {
    matches!(
        m,
        Motion::InnerWord
            | Motion::AWord
            | Motion::InnerBigWord
            | Motion::ABigWord
            | Motion::InnerParagraph
            | Motion::AParagraph
            | Motion::InnerSentence
            | Motion::ASentence
            | Motion::Pair { .. }
            | Motion::Quote { .. }
    )
}

impl EditorState {
    /// A fresh editor over `initial` bytes, cursor at the start, Normal mode, marked saved (loaded content).
    pub fn new(initial: impl Into<Vec<u8>>) -> EditorState {
        let mut doc = Document::new(DocumentId(1), initial);
        doc.mark_saved();
        EditorState {
            doc,
            cursor: 0,
            mode: Mode::Normal,
            last_was_edit: false,
            registers: RegisterStore::new(),
            pending_register: None,
            anchor: None,
            last_visual: None,
            // Schema defaults (spec/config-schema.yaml): editor.tab_width=4, editor.indent_style=space.
            // Runtime config wiring is deferred (as with editor.scrolloff), so the shift operators read
            // these fields, which currently always hold the defaults.
            tab_width: 4,
            indent_style: IndentStyle::Space,
        }
    }

    /// Set the indentation config the shift operators (`>>`/`<<`) use. Runtime config loading is deferred;
    /// until it lands this is the seam a loader (or a test) uses to install `editor.tab_width` /
    /// `editor.indent_style`. No new schema key — both are existing keys (spec/config-schema.yaml).
    pub fn set_indent(&mut self, tab_width: usize, indent_style: IndentStyle) {
        self.tab_width = tab_width.max(1);
        self.indent_style = indent_style;
    }

    /// One indent level as bytes: `tab_width` spaces (space style) or a single `\t` (tab style).
    fn indent_unit(&self) -> Vec<u8> {
        match self.indent_style {
            IndentStyle::Space => vec![b' '; self.tab_width],
            IndentStyle::Tab => vec![b'\t'],
        }
    }

    /// The unnamed register's current contents (for tests / a future `:registers`).
    #[must_use]
    pub fn register(&self) -> &Register {
        self.registers.unnamed()
    }

    /// The whole register store (for tests / a future `:registers`), giving access to the named slots.
    #[must_use]
    pub fn registers(&self) -> &RegisterStore {
        &self.registers
    }

    /// The highlighted byte range `[start, end)` of the current Visual selection, or `None` in Normal/Insert.
    /// Charwise includes the character under the active end; linewise spans whole lines. For the frontend to
    /// paint the selection.
    #[must_use]
    pub fn selection_span(&self) -> Option<(usize, usize)> {
        // Visual and Select paint the same selection (they share the anchor and toggle via CTRL-G).
        let line = self.mode.selection()?;
        Some(selection_range(
            self.bytes(),
            self.anchor?,
            self.cursor,
            line,
        ))
    }

    /// The document bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        self.doc.bytes()
    }

    /// The document as UTF-8, or `None` if not valid UTF-8.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        self.doc.as_str()
    }

    /// The cursor's byte offset (on a char boundary).
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The current mode.
    #[must_use]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Whether the buffer differs from what is on disk (delegates to the document's node-identity check).
    #[must_use]
    pub fn is_modified(&self) -> bool {
        self.doc.is_modified()
    }
}

/// The byte range `[s, e)` a `delete`/`yank` operator covers for a motion + count, plus whether the removed
/// span is **linewise** (its register geometry and paste shape). Linewise motions delete whole lines;
/// paragraph motions (`d}`) become linewise per Vim's exclusive-linewise rule; everything else is charwise.
fn op_span(b: &[u8], cur: usize, m: Motion, count: u32) -> (usize, usize, bool) {
    // Whole-lines span from the cursor's line through the target's line, inclusive of the final newline.
    let whole_lines = |t: usize| {
        let start = line_start(b, cur.min(t));
        let le = line_end(b, cur.max(t));
        let end = if le < b.len() { le + 1 } else { le };
        (start, end, true)
    };
    match m {
        // Line jumps (`dG`, `dgg`, `d{n}G`) are linewise across every line between the cursor and target.
        Motion::GotoLine | Motion::LastLine => whole_lines(motion::target(b, cur, m, count)),
        // Vertical motions under an operator are linewise (`dj` deletes this line and the next). A motion
        // that cannot move a line (`dj` on the last line) fails the operator entirely (Vim) — a no-op range.
        Motion::Up | Motion::Down => {
            let t = motion::target(b, cur, m, count);
            if line_start(b, t) == line_start(b, cur) {
                (cur, cur, true)
            } else {
                whole_lines(t)
            }
        }
        // `dd` / `{count}dd`: whole lines from the cursor's line down.
        Motion::Line => {
            let start = line_start(b, cur);
            let mut end = start;
            for _ in 0..count.max(1) {
                let le = line_end(b, end);
                if le < b.len() {
                    end = le + 1;
                } else {
                    end = le;
                    break;
                }
            }
            (start, end, true)
        }
        // Paragraph objects (`dip`/`dap`) are linewise (Vim); `char_span` already returns whole lines.
        Motion::InnerParagraph | Motion::AParagraph => {
            let (s, e) = motion::char_span(b, cur, m, count);
            (s, e, true)
        }
        // Paragraph motions (`d}`/`d{`) are exclusive charwise, but Vim's exclusive-linewise rule turns them
        // linewise when the exclusive end sits at column 0 and the start is at/before the first non-blank of
        // its line (the common `d}` case); otherwise the end pulls back to the previous line end (charwise).
        Motion::ParagraphFwd | Motion::ParagraphBack => {
            let t = motion::target(b, cur, m, count);
            let (lo, hi) = (cur.min(t), cur.max(t));
            if lo >= hi {
                (lo, lo, false)
            } else if hi > 0 && hi == line_start(b, hi) {
                if lo <= motion::first_non_blank(b, lo) {
                    (line_start(b, lo), hi, true)
                } else {
                    (lo, hi - 1, false)
                }
            } else {
                (lo, hi, false)
            }
        }
        // Everything else is the motion's charwise span.
        _ => {
            let (s, e) = motion::char_span(b, cur, m, count);
            (s, e, false)
        }
    }
}

/// The byte range a `change` operator covers: for `Motion::Line` it is the *content* of the line(s) (the
/// newline is kept so `cc` leaves an empty line to type into); else the same charwise span as delete.
fn change_range(b: &[u8], cur: usize, m: Motion, count: u32) -> (usize, usize) {
    // Line jumps under change keep the final newline, leaving an empty line to type into (as `cc` does).
    if matches!(m, Motion::GotoLine | Motion::LastLine) {
        let t = motion::target(b, cur, m, count);
        let start = line_start(b, cur.min(t));
        return (start, line_end(b, cur.max(t)));
    }
    if m != Motion::Line {
        return motion::char_span(b, cur, m, count);
    }
    let start = line_start(b, cur);
    let mut pos = cur;
    for _ in 1..count.max(1) {
        let le = line_end(b, pos);
        if le < b.len() {
            pos = le + 1;
        } else {
            break;
        }
    }
    (start, line_end(b, pos))
}

/// The byte range `[s, e)` a Visual selection covers, from its anchor and active (cursor) ends. Charwise
/// includes the character under the higher end (Vim's inclusive selection); linewise spans whole lines
/// including the trailing newline where present.
fn selection_range(b: &[u8], anchor: usize, cursor: usize, line: bool) -> (usize, usize) {
    let lo = anchor.min(cursor);
    let hi = anchor.max(cursor);
    if line {
        let start = line_start(b, lo);
        let le = line_end(b, hi);
        let end = if le < b.len() { le + 1 } else { le };
        (start, end)
    } else {
        // Inclusive of the char under `hi`.
        let end = if hi < b.len() {
            next_boundary(b, hi)
        } else {
            hi
        };
        (lo, end)
    }
}

/// Walk `count` char boundaries forward from `from`, never past `limit` (typically the line end).
/// Returns the end byte offset; fewer than `count` chars available stops at `limit` (Vim's EOL clamp).
fn advance_n(b: &[u8], from: usize, count: u32, limit: usize) -> usize {
    let mut end = from;
    for _ in 0..count {
        if end >= limit {
            break;
        }
        let nb = next_boundary(b, end).min(limit);
        if nb == end {
            break;
        }
        end = nb;
    }
    end
}

/// Like [`advance_n`] but reports whether the FULL `count` chars fit within `[from, limit)`. The bool
/// is false when fewer than `count` chars remain — the signal `{count}r` uses to become a clean no-op.
fn advance_n_checked(b: &[u8], from: usize, count: u32, limit: usize) -> (usize, bool) {
    let mut end = from;
    for _ in 0..count {
        if end >= limit {
            return (end, false);
        }
        end = next_boundary(b, end);
    }
    (end, true)
}

/// The pure decision for one command.
#[must_use]
pub fn plan(st: &EditorState, cmd: &Command) -> Plan {
    let b = st.bytes();
    let cur = st.cursor;
    let nop = |cursor: usize, mode: Mode| Plan {
        action: Action::Nop,
        cursor,
        mode,
        is_edit: false,
        effects: Vec::new(),
        set_register: None,
        set_anchor: None,
    };
    let edit = |edits: EditList, cursor: usize, mode: Mode, hint: GroupHint| Plan {
        action: Action::Txn { edits, hint },
        cursor,
        mode,
        is_edit: true,
        effects: Vec::new(),
        set_register: None,
        set_anchor: None,
    };
    // A delete/change that also captures the removed span into the unnamed register (Vim: `d`/`c`/`x` fill
    // the register). `linewise` picks the register's paste geometry.
    let edit_yank =
        |edits: EditList, cursor: usize, mode: Mode, hint: GroupHint, reg: Register| Plan {
            action: Action::Txn { edits, hint },
            cursor,
            mode,
            is_edit: true,
            effects: Vec::new(),
            set_register: Some(RegWrite::Edit(reg)),
            set_anchor: None,
        };
    // Capture `b[s..e]` as a register value with the given geometry.
    let captured = |s: usize, e: usize, linewise: bool| {
        let bytes = b[s..e].to_vec();
        if linewise {
            Register::linewise(bytes)
        } else {
            Register::charwise(bytes)
        }
    };
    let hint = if st.last_was_edit {
        GroupHint::Continue
    } else {
        GroupHint::BreakBefore
    };
    let one = |e: Edit| EditList::new(vec![e]).expect("single edit is always valid");

    match cmd {
        Command::MoveLeft => nop(prev_boundary(b, cur), st.mode),
        Command::MoveRight => nop(next_boundary(b, cur), st.mode),
        Command::MoveLineStart => nop(line_start(b, cur), st.mode),
        Command::MoveLineEnd => nop(line_end(b, cur), st.mode),
        Command::MoveUp => {
            let ls = line_start(b, cur);
            if ls == 0 {
                nop(cur, st.mode)
            } else {
                let prev_ls = line_start(b, ls - 1);
                nop(at_col(b, prev_ls, col_of(b, ls, cur)), st.mode)
            }
        }
        Command::MoveDown => {
            let le = line_end(b, cur);
            if le >= b.len() {
                nop(cur, st.mode)
            } else {
                let next_ls = le + 1;
                nop(
                    at_col(b, next_ls, col_of(b, line_start(b, cur), cur)),
                    st.mode,
                )
            }
        }
        Command::EnterInsert => nop(cur, Mode::Insert),
        Command::EnterInsertAfter => nop(next_boundary(b, cur), Mode::Insert),
        Command::InsertLineStart => nop(motion::first_non_blank(b, cur), Mode::Insert),
        Command::AppendLineEnd => nop(line_end(b, cur), Mode::Insert),
        Command::OpenBelow => {
            let le = line_end(b, cur);
            edit(
                one(Edit::insert(le, b"\n".to_vec())),
                le + 1,
                Mode::Insert,
                hint,
            )
        }
        Command::OpenAbove => {
            let ls = line_start(b, cur);
            edit(
                one(Edit::insert(ls, b"\n".to_vec())),
                ls,
                Mode::Insert,
                hint,
            )
        }
        Command::EnterNormal => {
            // Vim: leaving Insert nudges the cursor left one, but never before the line start. Leaving Visual
            // (Esc) just collapses the selection in place — no nudge.
            if st.mode == Mode::Insert {
                let ls = line_start(b, cur);
                let c = if cur > ls { prev_boundary(b, cur) } else { cur };
                nop(c, Mode::Normal)
            } else {
                nop(cur, Mode::Normal)
            }
        }
        Command::InsertChar(c) => {
            let mut buf = [0u8; 4];
            let bytes = c.encode_utf8(&mut buf).as_bytes().to_vec();
            let n = bytes.len();
            edit(one(Edit::insert(cur, bytes)), cur + n, Mode::Insert, hint)
        }
        Command::InsertNewline => edit(
            one(Edit::insert(cur, b"\n".to_vec())),
            cur + 1,
            Mode::Insert,
            hint,
        ),
        Command::DeleteBack => {
            if cur == 0 {
                nop(cur, st.mode)
            } else {
                let p = prev_boundary(b, cur);
                edit(one(Edit::delete(p, cur - p)), p, st.mode, hint)
            }
        }
        Command::DeleteUnder(count) => {
            // `{count}x`: delete `count` chars from the cursor, clamped at end-of-line (Vim). Fewer than
            // `count` chars left deletes to EOL. The removed span fills the unnamed register (charwise).
            let le = line_end(b, cur);
            let end = advance_n(b, cur, *count, le);
            if end <= cur {
                nop(cur, st.mode)
            } else {
                let reg = captured(cur, end, false);
                edit_yank(one(Edit::delete(cur, end - cur)), cur, st.mode, hint, reg)
            }
        }
        Command::ReplaceChar(count, c) => {
            // `{count}r{ch}`: replace `count` chars with `ch`. Per Vim it is a NO-OP if fewer than
            // `count` chars remain on the line (never a partial replace). Cursor lands on the last one.
            let le = line_end(b, cur);
            let (end, reached) = advance_n_checked(b, cur, *count, le);
            if !reached {
                nop(cur, st.mode)
            } else {
                let mut buf = [0u8; 4];
                let one_ch = c.encode_utf8(&mut buf).as_bytes();
                let mut bytes = Vec::with_capacity(one_ch.len() * *count as usize);
                for _ in 0..*count {
                    bytes.extend_from_slice(one_ch);
                }
                let last = cur + one_ch.len() * (*count as usize - 1);
                edit(
                    one(Edit::replace(cur, end - cur, bytes)),
                    last,
                    st.mode,
                    hint,
                )
            }
        }
        Command::ToggleCase(count) => {
            // `{count}~`: toggle the case of `count` chars, clamped at EOL, then leave the cursor past the
            // last toggled char (a Normal-mode edit, so `commit` clamps it back onto the last char at EOL).
            // Case-toggle by Unicode scalar (uppercase→lowercase, else lowercase→uppercase), so non-ASCII
            // letters flip too (`~` on "αβ" → "Αβ"); non-letters are consumed but left unchanged. The
            // toggled UTF-8 may differ in byte length from the source, so the cursor lands at `cur +
            // flipped.len()`, not the source end.
            let le = line_end(b, cur);
            let end = advance_n(b, cur, *count, le);
            if end <= cur {
                nop(cur, st.mode)
            } else {
                // `[cur, end)` is on char boundaries (`advance_n` walks boundaries), so it is valid UTF-8.
                let src =
                    std::str::from_utf8(&b[cur..end]).expect("cursor span is on char boundaries");
                let mut flipped: Vec<u8> = Vec::with_capacity(end - cur);
                for ch in src.chars() {
                    if ch.is_uppercase() {
                        for c in ch.to_lowercase() {
                            let mut buf = [0u8; 4];
                            flipped.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                        }
                    } else if ch.is_lowercase() {
                        for c in ch.to_uppercase() {
                            let mut buf = [0u8; 4];
                            flipped.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                        }
                    } else {
                        let mut buf = [0u8; 4];
                        flipped.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                    }
                }
                if flipped == b[cur..end] {
                    nop(end, st.mode) // nothing was a letter: `~` just moves right
                } else {
                    let cursor = cur + flipped.len();
                    edit(
                        one(Edit::replace(cur, end - cur, flipped)),
                        cursor,
                        st.mode,
                        hint,
                    )
                }
            }
        }
        Command::JoinLines => {
            // Join the current line with the next on a single space (Vim `J`). No-op on the last line.
            let le = line_end(b, cur);
            if le >= b.len() {
                nop(cur, st.mode)
            } else {
                // Delete the newline plus the next line's leading blanks, insert one space.
                let mut ws_end = le + 1;
                while ws_end < b.len() && (b[ws_end] == b' ' || b[ws_end] == b'\t') {
                    ws_end += 1;
                }
                edit(
                    one(Edit::replace(le, ws_end - le, b" ".to_vec())),
                    le,
                    st.mode,
                    hint,
                )
            }
        }
        Command::Move(count, m) => {
            // A text object issued in a selection mode (`viw`, `vi(`) sets BOTH ends: anchor at the object's
            // start, cursor on its last char (inclusive selection). A bare motion only moves the cursor.
            if st.mode.selection().is_some() && is_text_object(*m) {
                let (s, e) = motion::char_span(b, cur, *m, *count);
                if s >= e {
                    return nop(cur, st.mode);
                }
                return Plan {
                    action: Action::Nop,
                    cursor: prev_boundary(b, e),
                    mode: st.mode,
                    is_edit: false,
                    effects: Vec::new(),
                    set_register: None,
                    set_anchor: Some(s),
                };
            }
            nop(motion::target(b, cur, *m, *count), st.mode)
        }
        Command::Delete(count, m) => {
            let (s, e, linewise) = op_span(b, cur, *m, *count);
            if s >= e {
                nop(cur, st.mode)
            } else if linewise && e == b.len() && s > 0 && b[e - 1] != b'\n' {
                // Deleting the buffer's LAST line while earlier lines remain, where that line has no
                // trailing newline (so the span itself does not already eat one): Vim removes the line
                // entirely (not blank it in place), which means also dropping the newline that ends the
                // previous line, then moving the cursor up to the new last line. The register still holds
                // the line content linewise ("beta\n"), not the leading newline we splice away.
                // (`dG` on a newline-terminated buffer keeps its own trailing newline and takes the plain
                // branch, since the span already ends in `\n`.)
                let reg = captured(s, e, true);
                let del_start = s - 1; // the '\n' terminating the previous line (s is a line start, s > 0)
                let cursor = line_start(b, del_start);
                edit_yank(
                    one(Edit::delete(del_start, e - del_start)),
                    cursor,
                    st.mode,
                    hint,
                    reg,
                )
            } else {
                let reg = captured(s, e, linewise);
                edit_yank(one(Edit::delete(s, e - s)), s, st.mode, hint, reg)
            }
        }
        Command::Change(count, m) => {
            if *m == Motion::Line {
                // `cc` / `{count}cc` / `S`: a LINEWISE change. Vim keeps the leading indent of the first
                // line (autoindent-like, but here config-independent — the existing indent TEXT is
                // preserved), deletes the rest of the line content down through `count` lines, keeps the
                // trailing newline, and enters Insert at the end of the kept indent. The register captures
                // the whole affected line(s) LINEWISE, including their indent and trailing newline.
                let (ls, content_end) = change_range(b, cur, *m, *count);
                let indent_end = motion::first_non_blank(b, ls).min(content_end);
                // Register span: whole lines including the terminating newline where one is present.
                let reg_end = if content_end < b.len() && b[content_end] == b'\n' {
                    content_end + 1
                } else {
                    content_end
                };
                let reg = captured(ls, reg_end, true);
                if indent_end >= content_end {
                    // Nothing after the indent to delete (empty/blank line): keep the buffer, but still
                    // capture the register linewise and drop into Insert at the indent end.
                    Plan {
                        action: Action::Nop,
                        cursor: indent_end,
                        mode: Mode::Insert,
                        is_edit: false,
                        effects: Vec::new(),
                        set_register: Some(RegWrite::Edit(reg)),
                        set_anchor: None,
                    }
                } else {
                    edit_yank(
                        one(Edit::delete(indent_end, content_end - indent_end)),
                        indent_end,
                        Mode::Insert,
                        hint,
                        reg,
                    )
                }
            } else {
                let (s, e) = change_range(b, cur, *m, *count);
                if s >= e {
                    Plan {
                        action: Action::Nop,
                        cursor: s,
                        mode: Mode::Insert,
                        is_edit: false,
                        effects: Vec::new(),
                        set_register: None,
                        set_anchor: None,
                    }
                } else {
                    // The register captures the removed content charwise (a partial-line change like `c$`
                    // pastes inline).
                    let reg = captured(s, e, false);
                    edit_yank(one(Edit::delete(s, e - s)), s, Mode::Insert, hint, reg)
                }
            }
        }
        Command::Yank(count, m) => {
            let (s, e, linewise) = op_span(b, cur, *m, *count);
            if s >= e {
                nop(cur, st.mode)
            } else {
                // Yank captures without editing; Vim leaves the cursor at the start of the yanked span.
                let reg = captured(s, e, linewise);
                Plan {
                    action: Action::Nop,
                    cursor: s,
                    mode: st.mode,
                    is_edit: false,
                    effects: Vec::new(),
                    set_register: Some(RegWrite::Yank(reg)),
                    set_anchor: None,
                }
            }
        }
        Command::ShiftRight(count) => plan_shift(st, cur, *count, true, hint),
        Command::ShiftLeft(count) => plan_shift(st, cur, *count, false, hint),
        // Paste reads the pending register (`"xp`) or the unnamed slot; `commit` clears the pending slot.
        Command::Paste { after, count } => paste(
            b,
            cur,
            st.mode,
            st.registers.get(st.pending_register),
            *after,
            *count,
        ),
        // `"x` — install the one-shot pending register. A pure state set: no edit, no cursor/mode change.
        Command::SetRegister(name) => Plan {
            action: Action::SetPending(*name),
            cursor: cur,
            mode: st.mode,
            is_edit: false,
            effects: Vec::new(),
            set_register: None,
            set_anchor: None,
        },
        Command::EnterVisual { line } => nop(cur, Mode::Visual { line: *line }),
        // CTRL-G toggle: enter Select over the same selection. The anchor is preserved because both are
        // selection modes, so `commit` keeps it (see the (true, true) arm there).
        Command::EnterSelect { line } => nop(cur, Mode::Select { line: *line }),
        Command::ReselectVisual => match st.last_visual {
            // Restore the remembered selection: re-enter Visual with the stored kind, put the cursor on the
            // active end, and install the stored anchor (via `set_anchor`, which `commit` applies after its
            // enter-selection bookkeeping). No prior selection → a clean no-op (Vim rings the bell).
            Some((anchor, active, line)) => Plan {
                action: Action::Nop,
                cursor: active,
                mode: Mode::Visual { line },
                is_edit: false,
                effects: Vec::new(),
                set_register: None,
                set_anchor: Some(anchor),
            },
            None => nop(cur, st.mode),
        },
        Command::ReplaceSelection(c) => {
            // Select's `open/replace-selection`: delete the selection, insert the char, enter Insert.
            let line = matches!(
                st.mode,
                Mode::Visual { line: true } | Mode::Select { line: true }
            );
            let mut buf = [0u8; 4];
            let ins = c.encode_utf8(&mut buf).as_bytes().to_vec();
            let n = ins.len();
            match st.anchor {
                Some(anchor) => {
                    let (s, e) = selection_range(b, anchor, cur, line);
                    if s < e {
                        // The removed span fills the unnamed register, as a Visual/Normal delete does.
                        let reg = captured(s, e, line);
                        edit_yank(
                            one(Edit::replace(s, e - s, ins)),
                            s + n,
                            Mode::Insert,
                            hint,
                            reg,
                        )
                    } else {
                        edit(one(Edit::insert(s, ins)), s + n, Mode::Insert, hint)
                    }
                }
                // No anchor (not really in a selection): degrade to a plain insert-and-enter-Insert.
                None => edit(one(Edit::insert(cur, ins)), cur + n, Mode::Insert, hint),
            }
        }
        Command::SwapSelectionEnds => {
            // Visual/Select `o`: exchange the two ends. The cursor jumps to the anchor; the anchor becomes
            // the old cursor (`set_anchor`, which `commit` installs). The SAME text stays selected, but a
            // later bare motion now extends the OTHER end (it re-plans against the new anchor). Involutive.
            // Outside a selection (no anchor) it is a clean no-op.
            match (st.mode.selection(), st.anchor) {
                (Some(_), Some(anchor)) => Plan {
                    action: Action::Nop,
                    cursor: anchor,
                    mode: st.mode,
                    is_edit: false,
                    effects: Vec::new(),
                    set_register: None,
                    set_anchor: Some(cur),
                },
                _ => nop(cur, st.mode),
            }
        }
        Command::YankSelection | Command::DeleteSelection | Command::ChangeSelection => {
            let line = matches!(
                st.mode,
                Mode::Visual { line: true } | Mode::Select { line: true }
            );
            let Some(anchor) = st.anchor else {
                // Not in a selection (or no anchor) — drop back to Normal, do nothing.
                return nop(cur, Mode::Normal);
            };
            let (s, e) = selection_range(b, anchor, cur, line);
            let reg = captured(s, e, line);
            match cmd {
                // Yank leaves the buffer unchanged, cursor at the selection start (Vim), back to Normal.
                Command::YankSelection => Plan {
                    action: Action::Nop,
                    cursor: s,
                    mode: Mode::Normal,
                    is_edit: false,
                    effects: Vec::new(),
                    set_register: Some(RegWrite::Yank(reg)),
                    set_anchor: None,
                },
                Command::DeleteSelection if s < e => {
                    edit_yank(one(Edit::delete(s, e - s)), s, Mode::Normal, hint, reg)
                }
                Command::ChangeSelection if s < e => {
                    edit_yank(one(Edit::delete(s, e - s)), s, Mode::Insert, hint, reg)
                }
                // Empty selection: just leave Visual (Change still opens Insert).
                Command::ChangeSelection => nop(s, Mode::Insert),
                _ => nop(s, Mode::Normal),
            }
        }
        Command::SearchNext(pat) => {
            let m = crate::search::find_next(b, pat.as_bytes(), cur + 1).unwrap_or(cur);
            nop(m, st.mode)
        }
        Command::SearchPrev(pat) => {
            let m = crate::search::find_prev(b, pat.as_bytes(), cur).unwrap_or(cur);
            nop(m, st.mode)
        }
        // `/pat` as a motion: step forward to the `count`-th match (each step searches from just past the
        // last), then either move there (`Move`) or fold `[cursor, match)` into a charwise-exclusive edit
        // (`d/pat`/`c/pat`/`y/pat`). If no forward match lands past the cursor the operator aborts (Vim
        // rings the bell) — a clean no-op, never a reversed/empty edit.
        Command::Search { op, count, pattern } => {
            let mut pos = cur;
            for _ in 0..(*count).max(1) {
                match crate::search::find_next(b, pattern.as_bytes(), pos + 1) {
                    Some(m) => pos = m,
                    None => break,
                }
            }
            match op {
                SearchOp::Move => nop(pos, st.mode),
                _ if pos <= cur => nop(cur, st.mode),
                SearchOp::Delete => {
                    let reg = captured(cur, pos, false);
                    edit_yank(one(Edit::delete(cur, pos - cur)), cur, st.mode, hint, reg)
                }
                SearchOp::Change => {
                    let reg = captured(cur, pos, false);
                    edit_yank(
                        one(Edit::delete(cur, pos - cur)),
                        cur,
                        Mode::Insert,
                        hint,
                        reg,
                    )
                }
                SearchOp::Yank => {
                    let reg = captured(cur, pos, false);
                    Plan {
                        action: Action::Nop,
                        cursor: cur,
                        mode: st.mode,
                        is_edit: false,
                        effects: Vec::new(),
                        set_register: Some(RegWrite::Yank(reg)),
                        set_anchor: None,
                    }
                }
            }
        }
        Command::Undo => Plan {
            action: Action::Undo,
            cursor: cur,
            mode: Mode::Normal,
            is_edit: false,
            effects: Vec::new(),
            set_register: None,
            set_anchor: None,
        },
        Command::Redo => Plan {
            action: Action::Redo,
            cursor: cur,
            mode: Mode::Normal,
            is_edit: false,
            effects: Vec::new(),
            set_register: None,
            set_anchor: None,
        },
        Command::Save => Plan {
            action: Action::Nop,
            cursor: cur,
            mode: st.mode,
            is_edit: false,
            effects: vec![Effect::Save],
            set_register: None,
            set_anchor: None,
        },
        Command::Quit => Plan {
            action: Action::Nop,
            cursor: cur,
            mode: st.mode,
            is_edit: false,
            effects: vec![Effect::Quit],
            set_register: None,
            set_anchor: None,
        },
    }
}

/// How many leading whitespace bytes one `<<` removes from the line at `ls`: a single leading tab, else up
/// to `tab_width` leading spaces — one indent level, never crossing a non-blank char or the line end
/// (`le`). Style-agnostic on purpose: a space-configured buffer that happens to start with a tab still
/// unindents by that tab, matching Vim's "remove one shiftwidth of indent" for the common cases ruse models.
fn shift_left_remove(b: &[u8], ls: usize, le: usize, tab_width: usize) -> usize {
    if ls < le && b[ls] == b'\t' {
        return 1;
    }
    let mut n = 0;
    while n < tab_width && ls + n < le && b[ls + n] == b' ' {
        n += 1;
    }
    n
}

/// Plan a linewise shift (`>>` / `<<`) over `count` lines from the cursor's line down. `right` adds one
/// indent level to each line; `!right` removes up to one. Empty lines are never indented (Vim); the cursor
/// lands on the first non-blank of the cursor's (first) line, exactly as Vim leaves it. The register is
/// untouched. Edits are one-per-line at distinct line starts, so the [`EditList`] is disjoint by construction.
fn plan_shift(st: &EditorState, cur: usize, count: u32, right: bool, hint: GroupHint) -> Plan {
    let b = st.bytes();
    let first_ls = line_start(b, cur);
    let first_le = line_end(b, first_ls);
    let old_fnb = motion::first_non_blank(b, first_ls);
    let unit = st.indent_unit();

    let mut edits: Vec<Edit> = Vec::new();
    let mut first_removed = 0usize;
    let mut ls = first_ls;
    for i in 0..count.max(1) {
        let le = line_end(b, ls);
        if right {
            // Vim indents a whitespace-only line but never a truly EMPTY one (`ls == le`).
            if ls < le {
                edits.push(Edit::insert(ls, unit.clone()));
            }
        } else {
            let remove = shift_left_remove(b, ls, le, st.tab_width);
            if i == 0 {
                first_removed = remove;
            }
            if remove > 0 {
                edits.push(Edit::delete(ls, remove));
            }
        }
        if le >= b.len() {
            break; // no more lines — shifting fewer than `count` is fine (Vim clamps too).
        }
        ls = le + 1;
    }

    // Cursor: first non-blank of the FIRST shifted line, computed against the POST-edit buffer. Prepending
    // `unit` (all blanks) shifts that line's first non-blank right by `unit.len()`; a `<<` shifts it left by
    // the bytes removed. An empty line got no indent, so the cursor stays at its start. The all-blank-line
    // case (fnb past the last char) is pulled back onto the last char by `commit`'s Normal-mode clamp.
    let cursor = if right {
        if first_ls < first_le {
            old_fnb + unit.len()
        } else {
            first_ls
        }
    } else {
        old_fnb - first_removed
    };

    if edits.is_empty() {
        // Nothing to indent/unindent (e.g. `<<` at column 0, or `>>` on an empty line): a pure cursor move.
        return Plan {
            action: Action::Nop,
            cursor,
            mode: st.mode(),
            is_edit: false,
            effects: Vec::new(),
            set_register: None,
            set_anchor: None,
        };
    }
    let edits = EditList::new(edits)
        .expect("shift edits sit at distinct line starts, so they are disjoint");
    Plan {
        action: Action::Txn { edits, hint },
        cursor,
        mode: st.mode(),
        is_edit: true,
        effects: Vec::new(),
        set_register: None,
        set_anchor: None,
    }
}

/// Build the paste plan for `p` (after) / `P` (before) from the unnamed register. Charwise pastes insert
/// inline next to the cursor; linewise pastes open a whole new line below/above. An empty register is a
/// no-op. This is the paste-geometry semantic D-026 pins down for v0.
fn paste(b: &[u8], cur: usize, mode: Mode, reg: &Register, after: bool, count: u32) -> Plan {
    let nop = Plan {
        action: Action::Nop,
        cursor: cur,
        mode,
        is_edit: false,
        effects: Vec::new(),
        set_register: None,
        set_anchor: None,
    };
    if reg.is_empty() {
        return nop;
    }
    // `{count}p` pastes the register `count` times (Vim); the register itself is unchanged. The repeated
    // bytes are one contiguous insert, so the cursor math below (last pasted byte) still holds.
    let count = count.max(1) as usize;
    let repeat = |unit: &[u8]| unit.repeat(count);
    let one = |e: Edit| EditList::new(vec![e]).expect("single edit is always valid");
    let mk = |at: usize, bytes: Vec<u8>, cursor: usize| Plan {
        action: Action::Txn {
            edits: one(Edit::insert(at, bytes)),
            hint: GroupHint::BreakBefore,
        },
        cursor,
        mode: Mode::Normal,
        is_edit: true,
        effects: Vec::new(),
        set_register: None,
        set_anchor: None,
    };

    if reg.is_linewise() {
        // Linewise content is normalized to end with '\n'; `{count}p` stacks that many whole-line copies.
        let text = repeat(reg.text());
        if after {
            let le = line_end(b, cur);
            if le < b.len() {
                // Insert after the current line's newline: the stored "...\n" becomes a fresh line below.
                mk(le + 1, text, le + 1)
            } else {
                // Last line has no trailing newline: prepend one and drop the stored trailing newline so no
                // dangling blank line is created. Cursor lands at the start of the pasted line.
                let mut bytes = vec![b'\n'];
                bytes.extend_from_slice(text.strip_suffix(b"\n").unwrap_or(&text));
                mk(le, bytes, le + 1)
            }
        } else {
            let ls = line_start(b, cur);
            mk(ls, text, ls)
        }
    } else {
        // `{count}p` inserts that many copies inline. Vim's charwise-paste cursor rule splits on whether the
        // pasted text spans lines: single-line content leaves the cursor on the LAST pasted byte, but content
        // carrying a newline (e.g. a charwise Visual delete across a line boundary) leaves it on the FIRST
        // pasted byte — the start of the inserted run — not the last.
        let text = repeat(reg.text());
        let n = text.len();
        let multiline = text.contains(&b'\n');
        if after {
            // Insert after the cursor char; cursor lands on the first pasted byte for multi-line content,
            // else on the last pasted byte's boundary (Vim behavior).
            let at = if cur < b.len() {
                next_boundary(b, cur)
            } else {
                cur
            };
            let cursor = if multiline {
                at
            } else {
                (at + n).saturating_sub(1)
            };
            mk(at, text, cursor)
        } else {
            // Insert before the cursor; cursor lands on the first pasted byte for multi-line content, else
            // on the last pasted byte.
            let cursor = if multiline {
                cur
            } else {
                (cur + n).saturating_sub(1)
            };
            mk(cur, text, cursor)
        }
    }
}

/// Apply a plan to the state, returning the effects the frontend must perform.
pub fn commit(st: &mut EditorState, plan: Plan) -> Vec<Effect> {
    // `"x` sets the one-shot pending register and nothing else — it must NOT be cleared by the tail below
    // (that clear is what consumes the selection on the FOLLOWING command), so it returns early.
    if let Action::SetPending(name) = plan.action {
        st.pending_register = name;
        return plan.effects;
    }
    let entry_selection = st.mode.selection();
    let was_selection = entry_selection.is_some();
    let entry_cursor = st.cursor;
    let entry_anchor = st.anchor;
    match plan.action {
        Action::Txn { edits, hint } => {
            let txn = Transaction::new(st.doc.revision(), edits, TransactionOrigin::UserInput)
                .with_hint(hint);
            // The plan built the edits from the current buffer, so apply cannot be stale or out of range.
            st.doc
                .apply(txn)
                .expect("planned transaction applies cleanly");
        }
        Action::Undo => {
            st.doc.undo();
        }
        Action::Redo => {
            st.doc.redo();
        }
        Action::Nop => {}
        // Handled by the early return above; the buffer-mutating tail never runs for it.
        Action::SetPending(_) => unreachable!("SetPending is handled before the action match"),
    }
    // The cursor the plan computed is valid for the post-action buffer, except undo/redo which resize the
    // text unpredictably — clamp and snap to a char boundary either way.
    st.cursor = snap(st.doc.bytes(), plan.cursor);
    st.mode = plan.mode;
    st.last_was_edit = plan.is_edit;
    // Vim never rests the Normal-mode cursor on the newline: after an edit that leaves it beyond the final
    // char of a non-empty line, pull it back onto the last char (e.g. `dw` on the last word → the cursor
    // clamps to the trailing char rather than the line end). Scoped to edits in Normal mode so it never
    // touches Insert's legitimate cursor-past-end, and guarded by `ls < le` so an empty line keeps `[n,0]`.
    if plan.is_edit && st.mode == Mode::Normal {
        let b = st.doc.bytes();
        let le = line_end(b, st.cursor);
        let ls = line_start(b, st.cursor);
        if st.cursor == le && ls < le {
            st.cursor = prev_boundary(b, le);
        }
    }
    // A yank/delete/change writes its captured span into the pending register (or unnamed when none),
    // mirroring the unnamed slot on a named write; append (`"A`) is handled inside the store.
    match plan.set_register {
        Some(RegWrite::Edit(reg)) => st.registers.write(st.pending_register, reg),
        Some(RegWrite::Yank(reg)) => st.registers.yank(st.pending_register, reg),
        None => {}
    }
    // Maintain the selection anchor: set it when entering a selection mode (Visual/Select; the fixed end
    // is where the cursor was), keep it while staying in one — including across a Visual↔Select CTRL-G
    // toggle, since both are selection modes — and clear it on any exit to Normal/Insert.
    match (was_selection, st.mode.selection().is_some()) {
        (false, true) => st.anchor = Some(entry_cursor),
        (_, false) => {
            // Leaving a selection: remember it (anchor, active end, kind) for `gv` BEFORE dropping the
            // anchor — the depth-1 slice of D-027's `` `< ``/`` `> `` history. Only fires on an actual exit
            // (entry_selection is None when we were already in Normal), so a plain Normal command is inert.
            if let (Some(line), Some(a)) = (entry_selection, entry_anchor) {
                st.last_visual = Some((a, entry_cursor, line));
            }
            st.anchor = None;
        }
        (true, true) => {}
    }
    // A text object in a selection mode overrides the anchor to span the object (both ends move at once).
    if let Some(a) = plan.set_anchor {
        st.anchor = Some(a);
    }
    // Keep the raw-offset anchor valid: an edit applied while in Visual mode can resize the buffer under it,
    // and a stale anchor past the new end would make `selection_range` slice out of bounds (a core panic).
    // Snapping clamps it into range and onto a char boundary. The edit-tracking anchor-store position that
    // would move the anchor *semantically* with the edit is deferred (D-027); v0 only guarantees totality.
    if let Some(a) = st.anchor {
        st.anchor = Some(snap(st.doc.bytes(), a));
    }
    // The pending register (`"x`) is one-shot: any command other than `SetRegister` (which returned early
    // above) consumes it. Cleared here AFTER the register write so a stray `"x` before a non-register
    // command is simply forgotten, and a later plain edit never leaks into the named slot.
    st.pending_register = None;
    plan.effects
}

/// Convenience: plan then commit one command.
pub fn apply_command(st: &mut EditorState, cmd: &Command) -> Vec<Effect> {
    let p = plan(st, cmd);
    commit(st, p)
}

#[cfg(test)]
mod register_tests {
    use super::*;

    fn run(initial: &str, cmds: &[Command]) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        for c in cmds {
            apply_command(&mut st, c);
        }
        st
    }

    fn text(st: &EditorState) -> String {
        String::from_utf8(st.bytes().to_vec()).expect("utf8")
    }

    #[test]
    fn yy_then_p_duplicates_the_line_below() {
        let st = run(
            "aaa\nbbb\n",
            &[
                Command::Yank(1, Motion::Line),
                Command::Paste {
                    after: true,
                    count: 1,
                },
            ],
        );
        assert_eq!(text(&st), "aaa\naaa\nbbb\n");
    }

    #[test]
    fn dd_fills_register_and_p_pastes_it_below() {
        let st = run(
            "one\ntwo\n",
            &[
                Command::Delete(1, Motion::Line),
                Command::Paste {
                    after: true,
                    count: 1,
                },
            ],
        );
        assert_eq!(text(&st), "two\none\n");
    }

    #[test]
    fn xp_transposes_two_characters() {
        // The classic Vim idiom: `x` yanks the char, `p` puts it after the next one.
        let st = run(
            "abc",
            &[
                Command::DeleteUnder(1),
                Command::Paste {
                    after: true,
                    count: 1,
                },
            ],
        );
        assert_eq!(text(&st), "bac");
    }

    #[test]
    fn charwise_paste_after_inserts_past_the_cursor() {
        let st = run(
            "foo",
            &[
                Command::Yank(1, Motion::Right),
                Command::Paste {
                    after: true,
                    count: 1,
                },
            ],
        );
        assert_eq!(text(&st), "ffoo");
    }

    #[test]
    fn linewise_capital_p_pastes_above() {
        // Move onto line "y", yank it, then `P` duplicates it above.
        let st = run(
            "x\ny\n",
            &[
                Command::MoveDown,
                Command::Yank(1, Motion::Line),
                Command::Paste {
                    after: false,
                    count: 1,
                },
            ],
        );
        assert_eq!(text(&st), "x\ny\ny\n");
    }

    #[test]
    fn linewise_paste_on_last_line_without_trailing_newline() {
        // "b" has no trailing newline; the register normalizes it and paste-below adds a clean line.
        let st = run(
            "a\nb",
            &[
                Command::MoveDown,
                Command::Yank(1, Motion::Line),
                Command::Paste {
                    after: true,
                    count: 1,
                },
            ],
        );
        assert_eq!(text(&st), "a\nb\nb");
    }

    #[test]
    fn paste_from_empty_register_is_a_noop() {
        let st = run(
            "hello",
            &[Command::Paste {
                after: true,
                count: 1,
            }],
        );
        assert_eq!(text(&st), "hello");
        assert!(st.register().is_empty());
    }

    #[test]
    fn delete_updates_the_register_geometry() {
        // A charwise delete stores charwise; a linewise delete stores linewise.
        let st = run("word\n", &[Command::Delete(1, Motion::Right)]);
        assert!(!st.register().is_linewise());
        let st = run("word\n", &[Command::Delete(1, Motion::Line)]);
        assert!(st.register().is_linewise());
    }

    #[test]
    fn count_paste_repeats_charwise_register_inline() {
        // `yl2p` on "abc": yank "a", paste it twice after the cursor -> "aaabc", cursor on the last copy.
        let st = run(
            "abc",
            &[
                Command::Yank(1, Motion::Right),
                Command::Paste {
                    after: true,
                    count: 2,
                },
            ],
        );
        assert_eq!(text(&st), "aaabc");
        assert_eq!(st.cursor(), 2, "cursor lands on the last pasted byte");
        assert_eq!(
            st.register().text(),
            b"a",
            "the register is unchanged by paste"
        );
    }

    #[test]
    fn named_yank_writes_slot_and_mirrors_unnamed() {
        // `"ayiw` on "foo bar": yank "foo" into register a, which also mirrors the unnamed slot.
        let st = run(
            "foo bar",
            &[
                Command::SetRegister(Some('a')),
                Command::Yank(1, Motion::InnerWord),
            ],
        );
        assert_eq!(st.registers().get(Some('a')).text(), b"foo");
        assert_eq!(
            st.register().text(),
            b"foo",
            "unnamed mirrors the named write"
        );
    }

    #[test]
    fn named_paste_reads_the_named_slot() {
        // `"ayiw$"ap` on "foo bar" -> "foo barfoo" (the oracle fixture reg_named_yank_paste).
        let st = run(
            "foo bar",
            &[
                Command::SetRegister(Some('a')),
                Command::Yank(1, Motion::InnerWord),
                Command::Move(1, Motion::LineEnd),
                Command::SetRegister(Some('a')),
                Command::Paste {
                    after: true,
                    count: 1,
                },
            ],
        );
        assert_eq!(text(&st), "foo barfoo");
        assert_eq!(st.cursor(), 9);
    }

    #[test]
    fn plain_edit_does_not_leak_into_a_named_slot() {
        // After a named yank, a plain (unregistered) delete must write ONLY the unnamed slot — the pending
        // register is one-shot and cleared once consumed.
        let st = run(
            "foo bar",
            &[
                Command::SetRegister(Some('a')),
                Command::Yank(1, Motion::InnerWord), // a = unnamed = "foo"
                Command::DeleteUnder(1),             // plain x: unnamed only
            ],
        );
        assert_eq!(
            st.registers().get(Some('a')).text(),
            b"foo",
            "named slot untouched"
        );
        assert_eq!(st.register().text(), b"f", "plain x wrote unnamed only");
    }

    #[test]
    fn uppercase_register_appends() {
        // `"ayiw` then `"Ayiw` on the next word appends charwise -> "foobar" (matches the nvim oracle).
        let st = run(
            "foo bar",
            &[
                Command::SetRegister(Some('a')),
                Command::Yank(1, Motion::InnerWord),
                Command::Move(1, Motion::WordFwd),
                Command::SetRegister(Some('A')),
                Command::Yank(1, Motion::InnerWord),
            ],
        );
        assert_eq!(st.registers().get(Some('a')).text(), b"foobar");
        assert_eq!(st.register().text(), b"foobar");
    }

    #[test]
    fn stray_register_selection_is_forgotten_by_a_motion() {
        // `"a` then a bare motion (no operator) drops the selection; a later plain delete stays unnamed-only.
        let st = run(
            "foo bar",
            &[
                Command::SetRegister(Some('a')),
                Command::Move(1, Motion::Right), // consumes+clears the pending register
                Command::DeleteUnder(1),
            ],
        );
        assert!(
            st.registers().get(Some('a')).is_empty(),
            "named slot never written"
        );
    }

    #[test]
    fn cc_preserves_indent_and_captures_linewise() {
        // `cc` (Change over Motion::Line) keeps the first line's indent, deletes the rest, enters Insert
        // at the indent end, and captures the WHOLE line linewise (indent + trailing newline included).
        let st = run("  hello\nworld", &[Command::Change(1, Motion::Line)]);
        assert_eq!(text(&st), "  \nworld", "leading indent survives cc");
        assert_eq!(st.cursor(), 2, "cursor sits at the end of the kept indent");
        assert_eq!(st.mode(), Mode::Insert);
        assert!(st.register().is_linewise());
        assert_eq!(st.register().text(), b"  hello\n");
    }
}

#[cfg(test)]
mod visual_swap_tests {
    use super::*;

    fn run(initial: &str, cmds: &[Command]) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        for c in cmds {
            apply_command(&mut st, c);
        }
        st
    }

    fn text(st: &EditorState) -> String {
        String::from_utf8(st.bytes().to_vec()).expect("utf8")
    }

    #[test]
    fn swap_then_extend_then_delete() {
        // Parity fixture visual_o_swap_then_extend on "abcde": `lll`→col3, `v` anchors col3, `h`→col2
        // (sel "c"), `o` swaps (cursor col3, anchor col2), `l`→col4 (sel "cde"), `d` deletes it → "ab".
        let st = run(
            "abcde",
            &[
                Command::Move(1, Motion::Right),
                Command::Move(1, Motion::Right),
                Command::Move(1, Motion::Right),
                Command::EnterVisual { line: false },
                Command::Move(1, Motion::Left),
                Command::SwapSelectionEnds,
                Command::Move(1, Motion::Right),
                Command::DeleteSelection,
            ],
        );
        assert_eq!(text(&st), "ab");
        assert_eq!(st.register().text(), b"cde");
        assert!(!st.register().is_linewise());
        assert_eq!(st.mode(), Mode::Normal);
    }

    #[test]
    fn gv_reselects_the_last_visual_then_deletes_it() {
        // Parity fixture gv_reselect on "hello world": `viw` selects "hello", `y` yanks and leaves Visual,
        // `gv` re-selects the same span, `d` deletes it → " world". The selection survives the round-trip
        // to Normal because `y` captured it into last_visual on exit (D-027 depth-1).
        let st = run(
            "hello world",
            &[
                Command::EnterVisual { line: false },
                Command::Move(1, Motion::InnerWord),
                Command::YankSelection,
                Command::ReselectVisual,
                Command::DeleteSelection,
            ],
        );
        assert_eq!(text(&st), " world");
        assert_eq!(st.cursor(), 0);
        assert_eq!(st.register().text(), b"hello");
        assert!(!st.register().is_linewise());
        assert_eq!(st.mode(), Mode::Normal);
    }

    #[test]
    fn gv_without_a_prior_selection_is_a_noop() {
        let st = run("abc", &[Command::ReselectVisual]);
        assert_eq!(text(&st), "abc");
        assert_eq!(st.mode(), Mode::Normal);
        assert_eq!(st.cursor(), 0);
    }

    #[test]
    fn swap_is_involutive() {
        // `oo` restores the original selection span (both ends back where they were).
        let base = run(
            "abcde",
            &[
                Command::Move(1, Motion::Right),
                Command::Move(1, Motion::Right),
                Command::Move(1, Motion::Right),
                Command::EnterVisual { line: false },
                Command::Move(1, Motion::Left),
            ],
        );
        let span_before = base.selection_span();
        let cur_before = base.cursor();

        let mut st = base;
        apply_command(&mut st, &Command::SwapSelectionEnds);
        apply_command(&mut st, &Command::SwapSelectionEnds);
        assert_eq!(st.selection_span(), span_before, "oo restores the span");
        assert_eq!(st.cursor(), cur_before, "oo restores the active end");
    }

    #[test]
    fn swap_keeps_the_selected_span() {
        // A single `o` leaves the SAME text selected — only the active end changes.
        let mut st = run(
            "abcde",
            &[
                Command::Move(1, Motion::Right),
                Command::EnterVisual { line: false },
                Command::Move(1, Motion::Right),
                Command::Move(1, Motion::Right),
            ],
        );
        let span_before = st.selection_span();
        apply_command(&mut st, &Command::SwapSelectionEnds);
        assert_eq!(st.selection_span(), span_before, "swap preserves the span");
    }

    #[test]
    fn swap_outside_a_selection_is_a_noop() {
        let st = run("abcde", &[Command::SwapSelectionEnds]);
        assert_eq!(text(&st), "abcde");
        assert_eq!(st.cursor(), 0);
        assert_eq!(st.mode(), Mode::Normal);
        assert!(st.selection_span().is_none());
    }
}

#[cfg(test)]
mod single_key_edit_tests {
    use super::*;

    fn run(initial: &str, cmds: &[Command]) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        for c in cmds {
            apply_command(&mut st, c);
        }
        st
    }

    fn text(st: &EditorState) -> String {
        String::from_utf8(st.bytes().to_vec()).expect("utf8")
    }

    #[test]
    fn replace_char_keeps_the_cursor() {
        let st = run("abc", &[Command::MoveRight, Command::ReplaceChar(1, 'X')]);
        assert_eq!(text(&st), "aXc");
        assert_eq!(st.cursor(), 1, "r leaves the cursor on the replaced char");
        assert_eq!(st.mode(), Mode::Normal);
    }

    #[test]
    fn replace_char_multibyte() {
        let st = run("abc", &[Command::ReplaceChar(1, '가')]);
        assert_eq!(text(&st), "가bc");
    }

    #[test]
    fn replace_over_newline_or_eol_is_noop() {
        // On an empty line the cursor sits at the line end (== line start): `r` has no char to replace and
        // is a clean no-op (Vim). This is the Vim-valid way to land on EOL — a bare `$` rests on the last
        // char, never past it, so it can never park the cursor on the newline itself.
        let st = run("a\n\nb", &[Command::MoveDown, Command::ReplaceChar(1, 'X')]);
        assert_eq!(text(&st), "a\n\nb", "r on an empty line's EOL does nothing");
    }

    #[test]
    fn bare_dollar_lands_on_the_last_char_not_past_it() {
        // Vim: a bare `$` rests ON the last char (byte 4 of "hello"), unlike the `d$` operator span which
        // reaches the line end. This is what makes `$d0` leave the final char (parity fixture d_to_bol).
        let st = run("hello", &[Command::Move(1, Motion::LineEnd)]);
        assert_eq!(st.cursor(), 4);
        let st = run(
            "hello world",
            &[
                Command::Move(1, Motion::LineEnd),
                Command::Delete(1, Motion::LineStart),
            ],
        );
        assert_eq!(text(&st), "d", "$d0 deletes [BOL, last char)");
        assert_eq!(st.register().text(), b"hello worl");
        assert!(!st.register().is_linewise());
    }

    #[test]
    fn replace_char_with_count() {
        // `3rz` replaces three chars and leaves the cursor on the last one.
        let st = run("abcdef", &[Command::ReplaceChar(3, 'z')]);
        assert_eq!(text(&st), "zzzdef");
        assert_eq!(st.cursor(), 2, "cursor lands on the last replaced char");
        // Fewer than `count` chars remain on the line: a clean no-op (Vim never partial-replaces).
        let st = run("ab", &[Command::ReplaceChar(3, 'z')]);
        assert_eq!(text(&st), "ab");
        assert_eq!(st.cursor(), 0);
    }

    #[test]
    fn delete_under_with_count() {
        // `3x` deletes three chars into the unnamed register (charwise); clamps at EOL.
        let st = run("abcdef", &[Command::DeleteUnder(3)]);
        assert_eq!(text(&st), "def");
        assert_eq!(st.cursor(), 0);
        assert_eq!(st.register().text(), b"abc");
        assert!(!st.register().is_linewise());
        // Fewer than `count` chars left: delete to EOL (not across the newline).
        let st = run("abc\nxy", &[Command::DeleteUnder(9)]);
        assert_eq!(text(&st), "\nxy");
    }

    #[test]
    fn toggle_case_flips_and_moves_right() {
        let st = run("aBc", &[Command::ToggleCase(1)]);
        assert_eq!(text(&st), "ABc");
        assert_eq!(st.cursor(), 1);
        // On a non-letter, `~` just moves right.
        let st = run("1a", &[Command::ToggleCase(1)]);
        assert_eq!(text(&st), "1a");
        assert_eq!(st.cursor(), 1);
    }

    #[test]
    fn toggle_case_with_count() {
        // `3~` toggles three chars, leaving the cursor past the last (clamped at EOL).
        let st = run("abcdef", &[Command::ToggleCase(3)]);
        assert_eq!(text(&st), "ABCdef");
        assert_eq!(st.cursor(), 3);
        // Clamp: fewer than `count` chars left toggles to EOL. The cursor would land past the last char,
        // but Vim never rests the Normal-mode cursor on the newline, so `commit` pulls it onto the last char.
        let st = run("aB", &[Command::ToggleCase(9)]);
        assert_eq!(text(&st), "Ab");
        assert_eq!(st.cursor(), 1);
    }

    #[test]
    fn join_lines_uses_one_space_and_drops_indent() {
        let st = run("foo\n   bar", &[Command::JoinLines]);
        assert_eq!(text(&st), "foo bar");
        assert_eq!(st.cursor(), 3, "cursor lands on the joined space");
    }

    #[test]
    fn join_on_last_line_is_noop() {
        let st = run("only", &[Command::JoinLines]);
        assert_eq!(text(&st), "only");
    }
}

#[cfg(test)]
mod shift_tests {
    use super::*;

    fn run(initial: &str, cmds: &[Command]) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        for c in cmds {
            apply_command(&mut st, c);
        }
        st
    }

    fn run_indent(initial: &str, tw: usize, style: IndentStyle, cmds: &[Command]) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        st.set_indent(tw, style);
        for c in cmds {
            apply_command(&mut st, c);
        }
        st
    }

    fn text(st: &EditorState) -> String {
        String::from_utf8(st.bytes().to_vec()).expect("utf8")
    }

    #[test]
    fn shift_right_adds_tab_width_spaces_and_homes_to_first_non_blank() {
        // Default config = 4 spaces. Matches the parity fixture shift_right_line (oracle `>>`).
        let st = run("hello", &[Command::ShiftRight(1)]);
        assert_eq!(text(&st), "    hello");
        assert_eq!(st.cursor(), 4, "cursor lands on the first non-blank ('h')");
        assert_eq!(
            st.register().text(),
            b"",
            "shift does not touch the register"
        );
    }

    #[test]
    fn shift_right_stacks_onto_existing_indent() {
        let st = run("  hi", &[Command::ShiftRight(1)]);
        assert_eq!(text(&st), "      hi"); // 2 + 4 spaces
        assert_eq!(st.cursor(), 6);
    }

    #[test]
    fn shift_right_uses_a_tab_when_indent_style_is_tab() {
        let st = run_indent("hello", 4, IndentStyle::Tab, &[Command::ShiftRight(1)]);
        assert_eq!(text(&st), "\thello");
        assert_eq!(st.cursor(), 1, "cursor after the one-byte tab, on 'h'");
    }

    #[test]
    fn shift_right_leaves_a_truly_empty_line_untouched() {
        let st = run("", &[Command::ShiftRight(1)]);
        assert_eq!(text(&st), "", "Vim never indents an empty line");
        assert_eq!(st.cursor(), 0);
    }

    #[test]
    fn shift_right_count_shifts_multiple_lines_cursor_stays_on_first() {
        let st = run("a\nb\nc", &[Command::ShiftRight(2)]);
        assert_eq!(
            text(&st),
            "    a\n    b\nc",
            "2>> shifts the first two lines"
        );
        assert_eq!(
            st.cursor(),
            4,
            "cursor stays on the first line's first non-blank"
        );
    }

    #[test]
    fn shift_left_removes_one_level_of_spaces() {
        let st = run("    hello", &[Command::ShiftLeft(1)]);
        assert_eq!(text(&st), "hello");
        assert_eq!(st.cursor(), 0);
    }

    #[test]
    fn shift_left_removes_at_most_one_level() {
        // 6 leading spaces, tab_width 4 -> removes 4, leaves 2.
        let st = run("      hi", &[Command::ShiftLeft(1)]);
        assert_eq!(text(&st), "  hi");
        assert_eq!(st.cursor(), 2);
    }

    #[test]
    fn shift_left_partial_indent_never_crosses_column_zero() {
        let st = run("  hi", &[Command::ShiftLeft(1)]);
        assert_eq!(
            text(&st),
            "hi",
            "fewer than tab_width spaces: remove them all, no more"
        );
        assert_eq!(st.cursor(), 0);
    }

    #[test]
    fn shift_left_on_unindented_line_is_a_noop() {
        let st = run("hi", &[Command::ShiftLeft(1)]);
        assert_eq!(text(&st), "hi");
        assert_eq!(st.cursor(), 0);
    }

    #[test]
    fn shift_left_removes_a_leading_tab() {
        let st = run("\thello", &[Command::ShiftLeft(1)]);
        assert_eq!(text(&st), "hello");
        assert_eq!(st.cursor(), 0);
    }

    #[test]
    fn shift_right_then_left_round_trips() {
        let st = run("hello", &[Command::ShiftRight(1), Command::ShiftLeft(1)]);
        assert_eq!(text(&st), "hello", ">> then << restores the original");
        assert_eq!(st.cursor(), 0);
    }

    #[test]
    fn shift_is_undoable_as_one_edit() {
        let st = run("a\nb", &[Command::ShiftRight(2), Command::Undo]);
        assert_eq!(text(&st), "a\nb", "a single undo reverses the whole shift");
    }
}

#[cfg(test)]
mod insert_entry_tests {
    use super::*;

    fn run(initial: &str, cmds: &[Command]) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        for c in cmds {
            apply_command(&mut st, c);
        }
        st
    }

    fn text(st: &EditorState) -> String {
        String::from_utf8(st.bytes().to_vec()).expect("utf8")
    }

    #[test]
    fn open_below_inserts_a_line_and_enters_insert() {
        let st = run("ab\ncd", &[Command::OpenBelow, Command::InsertChar('X')]);
        assert_eq!(text(&st), "ab\nX\ncd");
        assert_eq!(st.mode(), Mode::Insert);
    }

    #[test]
    fn open_below_on_last_line() {
        let st = run("ab", &[Command::OpenBelow, Command::InsertChar('X')]);
        assert_eq!(text(&st), "ab\nX");
    }

    #[test]
    fn open_above_inserts_before_the_line() {
        // On line 2 ('cd'); O opens a line above it.
        let st = run(
            "ab\ncd",
            &[
                Command::MoveDown,
                Command::OpenAbove,
                Command::InsertChar('X'),
            ],
        );
        assert_eq!(text(&st), "ab\nX\ncd");
    }

    #[test]
    fn append_goes_to_line_end() {
        // On 'a' of "ab"; A appends at the end.
        let st = run(
            "ab\ncd",
            &[Command::AppendLineEnd, Command::InsertChar('X')],
        );
        assert_eq!(text(&st), "abX\ncd");
        assert_eq!(st.mode(), Mode::Insert);
    }

    #[test]
    fn insert_line_start_goes_to_first_non_blank() {
        // Cursor at end of a leading-blank line; I jumps to the first non-blank.
        let st = run(
            "  ab",
            &[
                Command::Move(1, Motion::LineEnd),
                Command::InsertLineStart,
                Command::InsertChar('X'),
            ],
        );
        assert_eq!(text(&st), "  Xab", "I inserts before the first non-blank");
    }
}

#[cfg(test)]
mod word_class_tests {
    use super::*;

    fn run(initial: &str, cmds: &[Command]) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        for c in cmds {
            apply_command(&mut st, c);
        }
        st
    }

    fn text(st: &EditorState) -> String {
        String::from_utf8(st.bytes().to_vec()).expect("utf8")
    }

    #[test]
    fn small_word_stops_at_punctuation() {
        // "foo.bar baz": w → '.', w → 'bar', w → 'baz'.
        let st = run("foo.bar baz", &[Command::Move(1, Motion::WordFwd)]);
        assert_eq!(st.cursor(), 3, "w stops on the '.'");
        let st = run(
            "foo.bar baz",
            &[
                Command::Move(1, Motion::WordFwd),
                Command::Move(1, Motion::WordFwd),
            ],
        );
        assert_eq!(st.cursor(), 4, "second w → start of 'bar'");
    }

    #[test]
    fn big_word_spans_punctuation() {
        // "foo.bar baz": W → 'baz' (foo.bar is one WORD).
        let st = run("foo.bar baz", &[Command::Move(1, Motion::BigWordFwd)]);
        assert_eq!(st.cursor(), 8);
    }

    #[test]
    fn small_word_back_treats_punct_as_a_word() {
        // cursor on 'b' of bar (4); b → the '.' word at 3.
        let st = run(
            "foo.bar",
            &[
                Command::Move(4, Motion::Right),
                Command::Move(1, Motion::WordBack),
            ],
        );
        assert_eq!(st.cursor(), 3);
    }

    #[test]
    fn dw_small_deletes_to_the_punctuation() {
        let st = run("foo.bar", &[Command::Delete(1, Motion::WordFwd)]);
        assert_eq!(text(&st), ".bar", "dw deletes 'foo' up to the '.'");
    }

    #[test]
    fn dbigw_deletes_the_whole_word() {
        let st = run("foo.bar baz", &[Command::Delete(1, Motion::BigWordFwd)]);
        assert_eq!(text(&st), "baz", "dW deletes 'foo.bar ' entirely");
    }

    #[test]
    fn multibyte_is_one_word() {
        // "가나 다": w skips the Hangul word to the next.
        let st = run("가나 다", &[Command::Move(1, Motion::WordFwd)]);
        assert_eq!(
            st.cursor(),
            7,
            "w lands on '다' after the multibyte word + space"
        );
    }

    #[test]
    fn whitespace_only_text_is_unchanged_from_word_style() {
        // The pre-existing WORD behavior is preserved for plain words.
        let st = run("foo bar baz", &[Command::Move(2, Motion::WordFwd)]);
        assert_eq!(st.cursor(), 8);
    }
}

#[cfg(test)]
mod bracket_match_tests {
    use super::*;

    fn run(initial: &str, cmds: &[Command]) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        for c in cmds {
            apply_command(&mut st, c);
        }
        st
    }

    fn text(st: &EditorState) -> String {
        String::from_utf8(st.bytes().to_vec()).expect("utf8")
    }

    fn pct() -> Command {
        Command::Move(1, Motion::MatchBracket)
    }

    #[test]
    fn jumps_between_a_pair_both_ways() {
        // "a(bc)d": '(' at 1, ')' at 4.
        let st = run("a(bc)d", &[Command::MoveRight, pct()]);
        assert_eq!(st.cursor(), 4, "from '(' to ')'");
        let st = run(
            "a(bc)d",
            &[Command::Move(4, Motion::Right), pct()], // cursor onto ')'
        );
        assert_eq!(st.cursor(), 1, "from ')' back to '('");
    }

    #[test]
    fn respects_nesting() {
        // "((x))": outer '(' at 0 ↔ ')' at 4; inner '(' at 1 ↔ ')' at 3.
        let st = run("((x))", &[pct()]);
        assert_eq!(st.cursor(), 4);
        let st = run("((x))", &[Command::MoveRight, pct()]);
        assert_eq!(st.cursor(), 3);
    }

    #[test]
    fn finds_first_bracket_forward_when_not_on_one() {
        // cursor at 0 ('a'), first bracket forward is '(' at 2, its match ')' at 5.
        let st = run("ab(cd)", &[pct()]);
        assert_eq!(st.cursor(), 5);
    }

    #[test]
    fn matches_across_lines() {
        let st = run("(\n)", &[pct()]);
        assert_eq!(st.cursor(), 2, "% matches across a newline");
    }

    #[test]
    fn matches_by_type_ignoring_other_brackets() {
        // "([)]": '(' at 0 matches ')' at 2, ignoring the '[' — same as Vim.
        let st = run("([)]", &[pct()]);
        assert_eq!(st.cursor(), 2);
    }

    #[test]
    fn d_percent_is_inclusive() {
        // On '(' (index 1); d% deletes "(bc)" inclusive → "ad".
        let st = run(
            "a(bc)d",
            &[Command::MoveRight, Command::Delete(1, Motion::MatchBracket)],
        );
        assert_eq!(text(&st), "ad");
    }

    #[test]
    fn unmatched_bracket_is_a_noop() {
        let st = run("a(b", &[Command::MoveRight, pct()]);
        assert_eq!(st.cursor(), 1, "no closer → no move");
    }
}

#[cfg(test)]
mod line_jump_tests {
    use super::*;

    fn run(initial: &str, cmds: &[Command]) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        for c in cmds {
            apply_command(&mut st, c);
        }
        st
    }

    fn text(st: &EditorState) -> String {
        String::from_utf8(st.bytes().to_vec()).expect("utf8")
    }

    #[test]
    fn gg_goes_to_first_line_first_non_blank() {
        // Start on line 3; gg → first non-blank of line 1 (past the two spaces).
        let st = run(
            "  abc\ndef\nghi",
            &[
                Command::MoveDown,
                Command::MoveDown,
                Command::Move(1, Motion::GotoLine),
            ],
        );
        assert_eq!(st.cursor(), 2, "gg lands on the first non-blank of line 1");
    }

    #[test]
    fn cap_g_goes_to_last_line() {
        let st = run("abc\ndef\nxyz", &[Command::Move(1, Motion::LastLine)]);
        assert_eq!(
            st.cursor(),
            8,
            "G lands on the start of the last line 'xyz'"
        );
    }

    #[test]
    fn count_g_goes_to_that_line() {
        // {2}G → line 2 ('def' starts at byte 4).
        let st = run("abc\ndef\nghi", &[Command::Move(2, Motion::GotoLine)]);
        assert_eq!(st.cursor(), 4);
    }

    #[test]
    fn dg_deletes_linewise_to_last_line() {
        // On line 2; dG deletes lines 2..end.
        let st = run(
            "one\ntwo\nthree\n",
            &[Command::MoveDown, Command::Delete(1, Motion::LastLine)],
        );
        assert_eq!(text(&st), "one\n");
    }

    #[test]
    fn dgg_deletes_linewise_to_first_line() {
        // On line 2; dgg deletes lines 1..=2.
        let st = run(
            "one\ntwo\nthree\n",
            &[Command::MoveDown, Command::Delete(1, Motion::GotoLine)],
        );
        assert_eq!(text(&st), "three\n");
    }

    #[test]
    fn count_beyond_end_clamps_to_last_line() {
        let st = run("a\nb\n", &[Command::Move(99, Motion::GotoLine)]);
        // line 99 doesn't exist → clamp to the last line (the empty line after the final newline).
        assert_eq!(st.cursor(), 4);
    }
}

#[cfg(test)]
mod find_char_tests {
    use super::*;

    fn find(ch: char, forward: bool, till: bool) -> Motion {
        Motion::FindChar { ch, forward, till }
    }

    fn run(initial: &str, cmds: &[Command]) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        for c in cmds {
            apply_command(&mut st, c);
        }
        st
    }

    fn text(st: &EditorState) -> String {
        String::from_utf8(st.bytes().to_vec()).expect("utf8")
    }

    #[test]
    fn f_lands_on_the_char_t_stops_before() {
        // "abcxef", cursor 0.
        let st = run("abcxef", &[Command::Move(1, find('x', true, false))]);
        assert_eq!(st.cursor(), 3, "fx lands on x");
        let st = run("abcxef", &[Command::Move(1, find('x', true, true))]);
        assert_eq!(st.cursor(), 2, "tx stops one before x");
    }

    #[test]
    fn count_finds_the_nth() {
        let st = run("axbxcx", &[Command::Move(2, find('x', true, false))]);
        assert_eq!(st.cursor(), 3, "2fx lands on the second x");
    }

    #[test]
    fn backward_find() {
        // "abxde", cursor at end (4). F x → lands on x (index 2).
        let st = run(
            "abxde",
            &[
                Command::MoveLineEnd,
                Command::Move(1, find('x', false, false)),
            ],
        );
        assert_eq!(st.cursor(), 2, "Fx searches backward onto x");
    }

    #[test]
    fn dfx_deletes_through_the_char_dtx_up_to_it() {
        let st = run("abcxef", &[Command::Delete(1, find('x', true, false))]);
        assert_eq!(text(&st), "ef", "dfx deletes through x");
        let st = run("abcxef", &[Command::Delete(1, find('x', true, true))]);
        assert_eq!(text(&st), "xef", "dtx deletes up to but not including x");
    }

    #[test]
    fn stays_within_the_line() {
        // The x on the next line must not be found from line 1.
        let st = run("abc\nxyz", &[Command::Move(1, find('x', true, false))]);
        assert_eq!(st.cursor(), 0, "f does not cross the newline");
    }

    #[test]
    fn multibyte_target() {
        // "a가b" — find the multibyte '가' (bytes 1..4).
        let st = run("a가b", &[Command::Move(1, find('가', true, false))]);
        assert_eq!(st.cursor(), 1, "lands on the multibyte char boundary");
    }

    #[test]
    fn missing_target_is_a_noop() {
        let st = run("abc", &[Command::Move(1, find('z', true, false))]);
        assert_eq!(st.cursor(), 0);
    }
}

#[cfg(test)]
mod visual_tests {
    use super::*;

    fn run(initial: &str, cmds: &[Command]) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        for c in cmds {
            apply_command(&mut st, c);
        }
        st
    }

    fn text(st: &EditorState) -> String {
        String::from_utf8(st.bytes().to_vec()).expect("utf8")
    }

    #[test]
    fn entering_visual_sets_a_collapsed_selection() {
        let st = run("hello", &[Command::EnterVisual { line: false }]);
        assert_eq!(st.mode(), Mode::Visual { line: false });
        // Anchor == cursor: the selection covers exactly the character under the caret (inclusive).
        assert_eq!(st.selection_span(), Some((0, 1)));
    }

    #[test]
    fn motion_extends_the_selection_and_stays_visual() {
        let st = run(
            "hello",
            &[
                Command::EnterVisual { line: false },
                Command::MoveRight,
                Command::MoveRight,
            ],
        );
        assert_eq!(st.mode(), Mode::Visual { line: false });
        assert_eq!(st.selection_span(), Some((0, 3)), "v + l + l selects 'hel'");
    }

    #[test]
    fn charwise_delete_over_selection() {
        let st = run(
            "hello",
            &[
                Command::EnterVisual { line: false },
                Command::MoveRight,
                Command::MoveRight,
                Command::DeleteSelection,
            ],
        );
        assert_eq!(text(&st), "lo");
        assert_eq!(st.mode(), Mode::Normal);
        assert_eq!(st.register().text(), b"hel");
        assert_eq!(st.selection_span(), None, "selection cleared on exit");
    }

    #[test]
    fn charwise_yank_then_paste() {
        let st = run(
            "hello",
            &[
                Command::EnterVisual { line: false },
                Command::MoveRight, // select "he"
                Command::YankSelection,
                Command::Paste {
                    after: true,
                    count: 1,
                },
            ],
        );
        // Yank leaves the buffer, cursor at selection start (0); `p` inserts "he" after the cursor char.
        assert_eq!(st.register().text(), b"he");
        assert_eq!(text(&st), "hheello");
    }

    #[test]
    fn linewise_delete_over_two_lines() {
        let st = run(
            "a\nb\nc\n",
            &[
                Command::EnterVisual { line: true },
                Command::MoveDown, // extend selection to the second line
                Command::DeleteSelection,
            ],
        );
        assert_eq!(text(&st), "c\n");
        assert!(st.register().is_linewise());
        assert_eq!(st.register().text(), b"a\nb\n");
    }

    #[test]
    fn change_selection_enters_insert() {
        let st = run(
            "hello",
            &[
                Command::EnterVisual { line: false },
                Command::MoveRight,
                Command::ChangeSelection,
            ],
        );
        assert_eq!(text(&st), "llo");
        assert_eq!(st.mode(), Mode::Insert);
    }

    #[test]
    fn esc_leaves_visual_without_editing() {
        let st = run(
            "hello",
            &[
                Command::EnterVisual { line: false },
                Command::MoveRight,
                Command::EnterNormal,
            ],
        );
        assert_eq!(text(&st), "hello");
        assert_eq!(st.mode(), Mode::Normal);
        assert_eq!(st.selection_span(), None);
    }
}

#[cfg(test)]
mod text_object_tests {
    use super::*;

    fn run(initial: &str, cmds: &[Command]) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        for c in cmds {
            apply_command(&mut st, c);
        }
        st
    }

    fn text(st: &EditorState) -> String {
        String::from_utf8(st.bytes().to_vec()).expect("utf8")
    }

    fn pair(open: char, close: char, around: bool) -> Motion {
        Motion::Pair {
            open,
            close,
            around,
        }
    }

    #[test]
    fn iw_splits_on_punctuation_but_big_word_does_not() {
        // cursor on 'f' of "foo.bar": `iw` is the word class run "foo"; `iW` is the whole WORD "foo.bar".
        let st = run("foo.bar", &[Command::Delete(1, Motion::InnerWord)]);
        assert_eq!(text(&st), ".bar", "diw stops at the punctuation");
        let st = run("foo.bar baz", &[Command::Delete(1, Motion::InnerBigWord)]);
        assert_eq!(text(&st), " baz", "diW spans the punctuation");
    }

    #[test]
    fn aw_and_a_big_word_take_trailing_whitespace() {
        let st = run("foo bar baz", &[Command::Delete(1, Motion::AWord)]);
        assert_eq!(
            text(&st),
            "bar baz",
            "daw removes the word and its trailing space"
        );
        let st = run("foo.bar baz", &[Command::Delete(1, Motion::ABigWord)]);
        assert_eq!(
            text(&st),
            "baz",
            "daW removes the WORD and its trailing space"
        );
    }

    #[test]
    fn delimiter_pair_inner_and_around() {
        // cursor inside the parens of "a(bc)d".
        let st = run(
            "a(bc)d",
            &[
                Command::Move(2, Motion::Right),
                Command::Delete(1, pair('(', ')', false)),
            ],
        );
        assert_eq!(text(&st), "a()d", "di( deletes the interior");
        let st = run(
            "a(bc)d",
            &[
                Command::Move(2, Motion::Right),
                Command::Delete(1, pair('(', ')', true)),
            ],
        );
        assert_eq!(text(&st), "ad", "da( deletes the delimiters too");
    }

    #[test]
    fn delimiter_pair_is_nesting_aware() {
        // On the inner content of "(a(b)c)": from the outer, di( takes everything inside the OUTER pair.
        let st = run("(a(b)c)", &[Command::Delete(1, pair('(', ')', false))]);
        assert_eq!(
            text(&st),
            "()",
            "di( on the opener spans to the matching closer"
        );
        // Cursor inside the inner pair selects only the inner interior.
        let st = run(
            "(a(b)c)",
            &[
                Command::Move(3, Motion::Right),
                Command::Delete(1, pair('(', ')', false)),
            ],
        );
        assert_eq!(
            text(&st),
            "(a()c)",
            "di( from inside the inner pair takes only 'b'"
        );
    }

    #[test]
    fn delimiter_object_outside_a_pair_is_a_noop() {
        let st = run("abc", &[Command::Delete(1, pair('(', ')', false))]);
        assert_eq!(text(&st), "abc");
    }

    #[test]
    fn quote_inner_and_around() {
        // `a "hi" b`: quotes at 2 and 5; cursor on 'h'.
        let st = run(
            "a \"hi\" b",
            &[
                Command::Move(3, Motion::Right),
                Command::Change(
                    1,
                    Motion::Quote {
                        ch: '"',
                        around: false,
                    },
                ),
            ],
        );
        assert_eq!(text(&st), "a \"\" b", "ci\" clears the interior");
        assert_eq!(st.mode(), Mode::Insert);
        let st = run(
            "a \"hi\" b",
            &[
                Command::Move(3, Motion::Right),
                Command::Delete(
                    1,
                    Motion::Quote {
                        ch: '"',
                        around: true,
                    },
                ),
            ],
        );
        assert_eq!(
            text(&st),
            "a b",
            "da\" removes the quotes and the trailing space"
        );
    }

    #[test]
    fn quotes_are_single_line() {
        // The quote on the next line must not pair with one on this line.
        let st = run(
            "x\"a\nb\"y",
            &[Command::Delete(
                1,
                Motion::Quote {
                    ch: '"',
                    around: false,
                },
            )],
        );
        assert_eq!(
            text(&st),
            "x\"a\nb\"y",
            "no matching quote on this line → no-op"
        );
    }

    #[test]
    fn paragraph_inner_and_around() {
        // "l1\nl2\n\nl3\n": cursor in the first paragraph.
        let st = run(
            "l1\nl2\n\nl3\n",
            &[Command::Delete(1, Motion::InnerParagraph)],
        );
        assert_eq!(text(&st), "\nl3\n", "dip removes the paragraph's lines");
        let st = run("l1\nl2\n\nl3\n", &[Command::Delete(1, Motion::AParagraph)]);
        assert_eq!(
            text(&st),
            "l3\n",
            "dap also removes the trailing blank line"
        );
    }

    #[test]
    fn sentence_inner_and_around() {
        let st = run("One. Two.", &[Command::Delete(1, Motion::InnerSentence)]);
        assert_eq!(
            text(&st),
            " Two.",
            "dis removes the first sentence, keeping the space"
        );
        let st = run("One. Two.", &[Command::Delete(1, Motion::ASentence)]);
        assert_eq!(
            text(&st),
            "Two.",
            "das removes the sentence and its trailing space"
        );
    }

    #[test]
    fn text_object_selects_in_visual() {
        // `viw` spans the word under the cursor.
        let st = run(
            "foo bar",
            &[
                Command::EnterVisual { line: false },
                Command::Move(1, Motion::InnerWord),
            ],
        );
        assert_eq!(st.mode(), Mode::Visual { line: false });
        assert_eq!(st.selection_span(), Some((0, 3)), "viw selects 'foo'");
        // `vi(` spans the interior of the enclosing pair.
        let st = run(
            "a(bc)d",
            &[
                Command::Move(2, Motion::Right),
                Command::EnterVisual { line: false },
                Command::Move(1, pair('(', ')', false)),
            ],
        );
        assert_eq!(st.selection_span(), Some((2, 4)), "vi( selects 'bc'");
    }
}

#[cfg(test)]
mod select_tests {
    use super::*;

    fn run(initial: &str, cmds: &[Command]) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        for c in cmds {
            apply_command(&mut st, c);
        }
        st
    }

    fn text(st: &EditorState) -> String {
        String::from_utf8(st.bytes().to_vec()).expect("utf8")
    }

    #[test]
    fn ctrl_g_toggles_visual_to_select_preserving_the_selection() {
        // v + l + l selects "hel"; CTRL-G (EnterSelect) keeps that exact span, now in Select.
        let st = run(
            "hello",
            &[
                Command::EnterVisual { line: false },
                Command::MoveRight,
                Command::MoveRight,
                Command::EnterSelect { line: false },
            ],
        );
        assert_eq!(st.mode(), Mode::Select { line: false });
        assert_eq!(
            st.selection_span(),
            Some((0, 3)),
            "selection survives the toggle"
        );
    }

    #[test]
    fn ctrl_g_toggles_select_back_to_visual_preserving_the_selection() {
        let st = run(
            "hello",
            &[
                Command::EnterVisual { line: false },
                Command::MoveRight,
                Command::EnterSelect { line: false },
                Command::EnterVisual { line: false },
            ],
        );
        assert_eq!(st.mode(), Mode::Visual { line: false });
        assert_eq!(
            st.selection_span(),
            Some((0, 2)),
            "toggling back keeps the span"
        );
    }

    #[test]
    fn printable_key_replaces_the_selection_and_enters_insert() {
        // Select "he", then a printable key deletes it, inserts the char, and drops into Insert.
        let st = run(
            "hello",
            &[
                Command::EnterVisual { line: false },
                Command::MoveRight,
                Command::EnterSelect { line: false },
                Command::ReplaceSelection('Z'),
            ],
        );
        assert_eq!(text(&st), "Zllo");
        assert_eq!(st.mode(), Mode::Insert);
        assert_eq!(
            st.cursor(),
            1,
            "cursor sits after the inserted char, ready to type"
        );
        assert_eq!(
            st.register().text(),
            b"he",
            "the replaced span fills the register"
        );
        assert_eq!(st.selection_span(), None);
    }

    #[test]
    fn replace_selection_multibyte() {
        let st = run(
            "hello",
            &[
                Command::EnterVisual { line: false },
                Command::MoveRight,
                Command::EnterSelect { line: false },
                Command::ReplaceSelection('가'),
            ],
        );
        assert_eq!(text(&st), "가llo");
        assert_eq!(st.mode(), Mode::Insert);
    }

    #[test]
    fn delete_on_a_select_selection_behaves_like_visual() {
        let st = run(
            "hello",
            &[
                Command::EnterVisual { line: false },
                Command::MoveRight,
                Command::MoveRight,
                Command::EnterSelect { line: false },
                Command::DeleteSelection,
            ],
        );
        assert_eq!(text(&st), "lo");
        assert_eq!(st.mode(), Mode::Normal);
        assert_eq!(st.register().text(), b"hel");
    }

    #[test]
    fn yank_on_a_select_selection_behaves_like_visual() {
        let st = run(
            "hello",
            &[
                Command::EnterVisual { line: false },
                Command::MoveRight,
                Command::EnterSelect { line: false },
                Command::YankSelection,
            ],
        );
        assert_eq!(text(&st), "hello");
        assert_eq!(st.mode(), Mode::Normal);
        assert_eq!(st.register().text(), b"he");
    }

    #[test]
    fn motion_extends_the_select_selection() {
        // A bare motion in Select moves the cursor and keeps the anchor — exactly as in Visual.
        let st = run(
            "hello",
            &[
                Command::EnterSelect { line: false },
                Command::MoveRight,
                Command::MoveRight,
            ],
        );
        assert_eq!(st.mode(), Mode::Select { line: false });
        assert_eq!(st.selection_span(), Some((0, 3)));
    }

    #[test]
    fn esc_leaves_select_without_editing() {
        let st = run(
            "hello",
            &[
                Command::EnterVisual { line: false },
                Command::MoveRight,
                Command::EnterSelect { line: false },
                Command::EnterNormal,
            ],
        );
        assert_eq!(text(&st), "hello");
        assert_eq!(st.mode(), Mode::Normal);
        assert_eq!(st.selection_span(), None);
    }
}
