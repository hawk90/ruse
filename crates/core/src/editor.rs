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

/// The editor mode. `Visual { line }` is a selection mode: charwise (`v`) or linewise (`V`). Blockwise
/// (`Ctrl-V`) is deferred. A selection is the pair `(anchor, cursor)`; a bare caret is the degenerate
/// collapsed selection, so extending to multi-selection later needs no type rewrite (D-027 trajectory).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Normal,
    Insert,
    Visual { line: bool },
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
        match self.mode {
            Mode::Visual { line } => Some(selection_range(
                self.bytes(),
                self.anchor?,
                self.cursor,
                line,
            )),
            _ => None,
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

/// The byte range a `delete` operator covers: linewise (whole lines incl. newline) for `Motion::Line`,
/// else the motion's charwise span.
fn op_range(b: &[u8], cur: usize, m: Motion, count: u32) -> (usize, usize) {
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
    };
    let edit = |edits: EditList, cursor: usize, mode: Mode, hint: GroupHint| Plan {
        action: Action::Txn { edits, hint },
        cursor,
        mode,
        is_edit: true,
        effects: Vec::new(),
        set_register: None,
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
        Command::Move(count, m) => nop(motion::target(b, cur, *m, *count), st.mode),
        Command::Delete(count, m) => {
            let (s, e) = op_range(b, cur, *m, *count);
            if s >= e {
                nop(cur, st.mode)
            } else {
                let reg = captured(s, e, *m == Motion::Line);
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
                let reg = captured(s, e, *m == Motion::Line);
                Plan {
                    action: Action::Nop,
                    cursor: s,
                    mode: st.mode,
                    is_edit: false,
                    effects: Vec::new(),
                    set_register: Some(reg),
                }
            }
        }
        Command::Paste { after } => paste(b, cur, st.mode, &st.register, *after),
        Command::EnterVisual { line } => nop(cur, Mode::Visual { line: *line }),
        Command::YankSelection | Command::DeleteSelection | Command::ChangeSelection => {
            let line = matches!(st.mode, Mode::Visual { line: true });
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
        },
        Command::Redo => Plan {
            action: Action::Redo,
            cursor: cur,
            mode: Mode::Normal,
            is_edit: false,
            effects: Vec::new(),
            set_register: None,
        },
        Command::Save => Plan {
            action: Action::Nop,
            cursor: cur,
            mode: st.mode,
            is_edit: false,
            effects: vec![Effect::Save],
            set_register: None,
        },
        Command::Quit => Plan {
            action: Action::Nop,
            cursor: cur,
            mode: st.mode,
            is_edit: false,
            effects: vec![Effect::Quit],
            set_register: None,
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
    let was_visual = matches!(st.mode, Mode::Visual { .. });
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
    // Maintain the Visual selection anchor: set it when entering Visual (the fixed end is where the cursor
    // was), keep it while staying in Visual, clear it on any exit to Normal/Insert.
    match (was_visual, matches!(st.mode, Mode::Visual { .. })) {
        (false, true) => st.anchor = Some(entry_cursor),
        (_, false) => st.anchor = None,
        (true, true) => {}
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
