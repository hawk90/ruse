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
use crate::transaction::{GroupHint, Transaction, TransactionOrigin};

/// The editor mode (v0: the two that matter for the spine).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Normal,
    Insert,
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
        }
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

// --- char-boundary + line helpers over the byte buffer (assumed valid UTF-8) ---

fn prev_boundary(b: &[u8], pos: usize) -> usize {
    let mut i = pos.min(b.len());
    while i > 0 {
        i -= 1;
        if is_boundary(b, i) {
            return i;
        }
    }
    0
}

fn next_boundary(b: &[u8], pos: usize) -> usize {
    let mut i = (pos + 1).min(b.len());
    while i < b.len() && !is_boundary(b, i) {
        i += 1;
    }
    i
}

fn is_boundary(b: &[u8], i: usize) -> bool {
    i == 0 || i == b.len() || (b[i] & 0xC0) != 0x80
}

fn snap(b: &[u8], pos: usize) -> usize {
    let p = pos.min(b.len());
    if is_boundary(b, p) {
        p
    } else {
        prev_boundary(b, p)
    }
}

fn line_start(b: &[u8], pos: usize) -> usize {
    b[..pos.min(b.len())]
        .iter()
        .rposition(|&c| c == b'\n')
        .map_or(0, |i| i + 1)
}

fn line_end(b: &[u8], pos: usize) -> usize {
    let p = pos.min(b.len());
    b[p..]
        .iter()
        .position(|&c| c == b'\n')
        .map_or(b.len(), |i| p + i)
}

/// Char count from `start` to `pos` (the column when `start` is a line start).
fn col_of(b: &[u8], start: usize, pos: usize) -> usize {
    std::str::from_utf8(&b[start..pos])
        .map(|s| s.chars().count())
        .unwrap_or(0)
}

/// The byte offset `col` chars into the line beginning at `start`, clamped to the line's end.
fn at_col(b: &[u8], start: usize, col: usize) -> usize {
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
    };
    let edit = |edits: EditList, cursor: usize, mode: Mode, hint: GroupHint| Plan {
        action: Action::Txn { edits, hint },
        cursor,
        mode,
        is_edit: true,
        effects: Vec::new(),
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
        Command::EnterNormal => {
            // Vim: leaving Insert nudges the cursor left one, but never before the line start.
            let ls = line_start(b, cur);
            let c = if cur > ls { prev_boundary(b, cur) } else { cur };
            nop(c, Mode::Normal)
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
                edit(one(Edit::delete(cur, nb - cur)), cur, st.mode, hint)
            }
        }
        Command::Undo => Plan {
            action: Action::Undo,
            cursor: cur,
            mode: Mode::Normal,
            is_edit: false,
            effects: Vec::new(),
        },
        Command::Redo => Plan {
            action: Action::Redo,
            cursor: cur,
            mode: Mode::Normal,
            is_edit: false,
            effects: Vec::new(),
        },
        Command::Save => Plan {
            action: Action::Nop,
            cursor: cur,
            mode: st.mode,
            is_edit: false,
            effects: vec![Effect::Save],
        },
        Command::Quit => Plan {
            action: Action::Nop,
            cursor: cur,
            mode: st.mode,
            is_edit: false,
            effects: vec![Effect::Quit],
        },
    }
}

/// Apply a plan to the state, returning the effects the frontend must perform.
pub fn commit(st: &mut EditorState, plan: Plan) -> Vec<Effect> {
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
    plan.effects
}

/// Convenience: plan then commit one command.
pub fn apply_command(st: &mut EditorState, cmd: &Command) -> Vec<Effect> {
    let p = plan(st, cmd);
    commit(st, p)
}
