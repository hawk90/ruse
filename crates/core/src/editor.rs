//! The editor state and the pure **plan / commit** command pipeline (RFC-0012).
//!
//! [`plan`] is a *pure decision*: `(&EditorState, &Command) -> Plan`, no mutation, no IO. [`commit`] applies
//! a `Plan` and returns the [`Effect`]s the frontend must perform. Because the core never does IO, replaying
//! the same commands on the same initial document is deterministic (see [`crate::trace`]). This is the split
//! that captures most of a Haskell rewrite's benefit in Rust — enforced by an empty dependency set.

use crate::command::Command;
use crate::document::{Document, DocumentId};
use crate::edit::{Edit, EditList};
use crate::effect::Effect;
use crate::motion::{
    self, at_col, col_of, line_end, line_start, next_boundary, prev_boundary, snap, Motion,
};
use crate::register::Register;
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
    /// The v0 unnamed register: text yanked (`y`) or deleted (`d`/`c`/`x`) for a later paste (`p`/`P`).
    /// Named slots / the numbered ring are deferred (D-026; see `register.rs`).
    register: Register,
    /// The Visual-mode selection anchor (the fixed end; the cursor is the moving end). `Some` exactly while
    /// in `Mode::Visual`. The full anchor-store-backed `Selection` set is deferred (D-027).
    anchor: Option<usize>,
}

enum Action {
    Txn { edits: EditList, hint: GroupHint },
    Undo,
    Redo,
    Nop,
}

/// The pure result of [`plan`]: what a command would do, before any mutation.
pub struct Plan {
    action: Action,
    cursor: usize,
    mode: Mode,
    is_edit: bool,
    effects: Vec<Effect>,
    /// Text to store into the unnamed register on commit (a yank, or the text a delete/change removed).
    set_register: Option<Register>,
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
            register: Register::default(),
            anchor: None,
        }
    }

    /// The unnamed register's current contents (for tests / a future `:registers`).
    #[must_use]
    pub fn register(&self) -> &Register {
        &self.register
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

/// The byte range a `delete` operator covers: linewise (whole lines incl. newline) for `Motion::Line`,
/// else the motion's charwise span.
fn op_range(b: &[u8], cur: usize, m: Motion, count: u32) -> (usize, usize) {
    // Line jumps (`dG`, `dgg`, `d{n}G`) are linewise across every line between the cursor and the target.
    if matches!(m, Motion::GotoLine | Motion::LastLine) {
        let t = motion::target(b, cur, m, count);
        let start = line_start(b, cur.min(t));
        let le = line_end(b, cur.max(t));
        let end = if le < b.len() { le + 1 } else { le };
        return (start, end);
    }
    if m != Motion::Line {
        return motion::char_span(b, cur, m, count);
    }
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
    (start, end)
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
            set_register: Some(reg),
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
        Command::DeleteUnder => {
            if cur >= b.len() {
                nop(cur, st.mode)
            } else {
                let nb = next_boundary(b, cur);
                // Vim: `x` yanks the deleted character(s) into the unnamed register (charwise).
                let reg = captured(cur, nb, false);
                edit_yank(one(Edit::delete(cur, nb - cur)), cur, st.mode, hint, reg)
            }
        }
        Command::ReplaceChar(c) => {
            // Replace the char under the cursor; a no-op at end-of-line / empty buffer or over a newline.
            if cur >= b.len() || b[cur] == b'\n' {
                nop(cur, st.mode)
            } else {
                let nb = next_boundary(b, cur);
                let mut buf = [0u8; 4];
                let bytes = c.encode_utf8(&mut buf).as_bytes().to_vec();
                edit(one(Edit::replace(cur, nb - cur, bytes)), cur, st.mode, hint)
            }
        }
        Command::ToggleCase => {
            // Toggle the ASCII case of the char under the cursor (if a letter), then move right (Vim `~`).
            if cur >= b.len() || b[cur] == b'\n' {
                nop(cur, st.mode)
            } else {
                let nb = next_boundary(b, cur);
                if b[cur].is_ascii_alphabetic() {
                    let flipped = vec![b[cur] ^ 0b0010_0000]; // ASCII case bit
                    edit(one(Edit::replace(cur, 1, flipped)), nb, st.mode, hint)
                } else {
                    nop(nb, st.mode) // non-letter: `~` just moves right
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
            let (s, e) = op_range(b, cur, *m, *count);
            if s >= e {
                nop(cur, st.mode)
            } else {
                let reg = captured(
                    s,
                    e,
                    matches!(m, Motion::Line | Motion::GotoLine | Motion::LastLine),
                );
                edit_yank(one(Edit::delete(s, e - s)), s, st.mode, hint, reg)
            }
        }
        Command::Change(count, m) => {
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
                // `change_range` keeps the trailing newline on the buffer for `cc`; the register still
                // captures the removed content, charwise (Vim treats `cc`'s register as characterwise-ish;
                // v0 keeps it charwise — the common paste target is inline).
                let reg = captured(s, e, false);
                edit_yank(one(Edit::delete(s, e - s)), s, Mode::Insert, hint, reg)
            }
        }
        Command::Yank(count, m) => {
            let (s, e) = op_range(b, cur, *m, *count);
            if s >= e {
                nop(cur, st.mode)
            } else {
                // Yank captures without editing; Vim leaves the cursor at the start of the yanked span.
                let reg = captured(
                    s,
                    e,
                    matches!(m, Motion::Line | Motion::GotoLine | Motion::LastLine),
                );
                Plan {
                    action: Action::Nop,
                    cursor: s,
                    mode: st.mode,
                    is_edit: false,
                    effects: Vec::new(),
                    set_register: Some(reg),
                    set_anchor: None,
                }
            }
        }
        Command::Paste { after } => paste(b, cur, st.mode, &st.register, *after),
        Command::EnterVisual { line } => nop(cur, Mode::Visual { line: *line }),
        // CTRL-G toggle: enter Select over the same selection. The anchor is preserved because both are
        // selection modes, so `commit` keeps it (see the (true, true) arm there).
        Command::EnterSelect { line } => nop(cur, Mode::Select { line: *line }),
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
                    set_register: Some(reg),
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

/// Build the paste plan for `p` (after) / `P` (before) from the unnamed register. Charwise pastes insert
/// inline next to the cursor; linewise pastes open a whole new line below/above. An empty register is a
/// no-op. This is the paste-geometry semantic D-026 pins down for v0.
fn paste(b: &[u8], cur: usize, mode: Mode, reg: &Register, after: bool) -> Plan {
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
        // Linewise content is normalized to end with '\n'.
        let text = reg.text().to_vec();
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
        let text = reg.text().to_vec();
        let n = text.len();
        if after {
            // Insert after the cursor char; cursor ends on the last pasted byte's boundary (Vim behavior).
            let at = if cur < b.len() {
                next_boundary(b, cur)
            } else {
                cur
            };
            let end = at + n;
            mk(at, text, end.saturating_sub(1))
        } else {
            // Insert before the cursor; cursor ends on the last pasted byte.
            mk(cur, text, (cur + n).saturating_sub(1))
        }
    }
}

/// Apply a plan to the state, returning the effects the frontend must perform.
pub fn commit(st: &mut EditorState, plan: Plan) -> Vec<Effect> {
    let was_selection = st.mode.selection().is_some();
    let entry_cursor = st.cursor;
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
    }
    // The cursor the plan computed is valid for the post-action buffer, except undo/redo which resize the
    // text unpredictably — clamp and snap to a char boundary either way.
    st.cursor = snap(st.doc.bytes(), plan.cursor);
    st.mode = plan.mode;
    st.last_was_edit = plan.is_edit;
    if let Some(reg) = plan.set_register {
        st.register = reg;
    }
    // Maintain the selection anchor: set it when entering a selection mode (Visual/Select; the fixed end
    // is where the cursor was), keep it while staying in one — including across a Visual↔Select CTRL-G
    // toggle, since both are selection modes — and clear it on any exit to Normal/Insert.
    match (was_selection, st.mode.selection().is_some()) {
        (false, true) => st.anchor = Some(entry_cursor),
        (_, false) => st.anchor = None,
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
                Command::Paste { after: true },
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
                Command::Paste { after: true },
            ],
        );
        assert_eq!(text(&st), "two\none\n");
    }

    #[test]
    fn xp_transposes_two_characters() {
        // The classic Vim idiom: `x` yanks the char, `p` puts it after the next one.
        let st = run(
            "abc",
            &[Command::DeleteUnder, Command::Paste { after: true }],
        );
        assert_eq!(text(&st), "bac");
    }

    #[test]
    fn charwise_paste_after_inserts_past_the_cursor() {
        let st = run(
            "foo",
            &[
                Command::Yank(1, Motion::Right),
                Command::Paste { after: true },
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
                Command::Paste { after: false },
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
                Command::Paste { after: true },
            ],
        );
        assert_eq!(text(&st), "a\nb\nb");
    }

    #[test]
    fn paste_from_empty_register_is_a_noop() {
        let st = run("hello", &[Command::Paste { after: true }]);
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
        let st = run("abc", &[Command::MoveRight, Command::ReplaceChar('X')]);
        assert_eq!(text(&st), "aXc");
        assert_eq!(st.cursor(), 1, "r leaves the cursor on the replaced char");
        assert_eq!(st.mode(), Mode::Normal);
    }

    #[test]
    fn replace_char_multibyte() {
        let st = run("abc", &[Command::ReplaceChar('가')]);
        assert_eq!(text(&st), "가bc");
    }

    #[test]
    fn replace_over_newline_or_eol_is_noop() {
        let st = run(
            "ab\nc",
            &[Command::Move(1, Motion::LineEnd), Command::ReplaceChar('X')],
        );
        assert_eq!(text(&st), "ab\nc", "r on the line-end newline does nothing");
    }

    #[test]
    fn toggle_case_flips_and_moves_right() {
        let st = run("aBc", &[Command::ToggleCase]);
        assert_eq!(text(&st), "ABc");
        assert_eq!(st.cursor(), 1);
        // On a non-letter, `~` just moves right.
        let st = run("1a", &[Command::ToggleCase]);
        assert_eq!(text(&st), "1a");
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
                Command::Paste { after: true },
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
