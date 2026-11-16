//! The Insert-session bookkeeping [`super::apply_command`] keeps around a command: what an opened line
//! arms (smartindent, a comment leader, the indent `^ CTRL-D` took), the pending autoindent, the line `U`
//! restores, and Vim's `Insstart`. Split out of `editor/commit.rs` (#1504).

use super::super::{indent, planner, range, EditorState, Mode};
use super::is_text_entry;
use crate::command::{Command, OpenKind};
use crate::editor::CaretGravity;
use crate::motion::{col_of, line_end, line_start};

/// What a line the command opens carries, read from the buffer before the command's edit.
pub(super) struct Opened {
    /// The smartindent state the opened line arms.
    si: indent::SiState,
    /// Whether the open continues a comment leader, which counts as an untouched autoindent (nvim sets
    /// `did_ai`).
    pub(super) leader: bool,
    /// The end of a three-piece comment the opened line may close.
    end_comment: Option<u8>,
    /// The indent `^ CTRL-D` deletes, for the next `<CR>`.
    deleted_indent: usize,
}

impl Opened {
    pub(super) fn of(st: &EditorState, cmd: &Command) -> Opened {
        let deleted_indent = if matches!(cmd, Command::InsertDeleteIndentRestore) {
            let b = st.doc.bytes();
            let ls = line_start(b, st.view.cursor);
            // The `^` is deleted first, by the `DeleteBack` before this command; the indent is the line's.
            indent::indent_cols(&b[ls..line_end(b, ls)], st.view.indent.tabs()).0
        } else {
            0
        };
        Opened {
            si: opened_si_state(st, cmd),
            leader: planner::open_comment_line(st, cmd).is_some_and(|o| o.1),
            end_comment: planner::open_end_comment(st, cmd),
            deleted_indent,
        }
    }
}

/// Opening a line drops the line `U` would restore (nvim's `open_line`). After `o`/`O` the first typed
/// change still saves the new line; after an Insert `<CR>` the rest of the insert saves nothing.
pub(super) fn drop_opened_line_undo(
    st: &mut EditorState,
    cmd: &Command,
    entry_rev: crate::Revision,
) {
    match cmd {
        Command::OpenBelow
        | Command::OpenAbove
        | Command::OpenLineIndent {
            kind: OpenKind::Below | OpenKind::Above,
            ..
        } => st.doc.clear_line_undo(false),
        Command::InsertNewline
        | Command::OpenLineIndent {
            kind: OpenKind::Split,
            ..
        } if st.doc.revision() != entry_rev => st.doc.clear_line_undo(true),
        _ => {}
    }
}

/// The Insert-session state a command leaves: the pending autoindent, smartindent, the indent `^ CTRL-D`
/// took, the comment end to close and whether a space was just typed.
pub(super) fn set_insert_state(
    st: &mut EditorState,
    cmd: &Command,
    opened: &Opened,
    linewise_change: bool,
    entry_text: &[u8],
    entry_cursor: usize,
) {
    // Maintain the auto-indent-pending flag (read by `EnterNormal` for `<Esc>` autoindent cleanup): a
    // tree-suggested open sets it; typing a non-blank clears it; any Insert entry/exit resets it so it
    // never leaks to a later line. Done here (not in the pure planner) because it depends on the Command.
    match cmd {
        Command::OpenLineIndent { level, .. } => st.view.auto_indent_pending = *level > 0,
        Command::OpenBelow | Command::OpenAbove | Command::InsertNewline => {
            st.view.auto_indent_pending =
                st.view.mode == Mode::Insert && (opened.leader || only_indent_before_caret(st));
        }
        _ if linewise_change => st.view.auto_indent_pending = only_indent_before_caret(st),
        // A caret key off the line ends it (nvim's `start_arrow`).
        _ if left_line(entry_text, entry_cursor, st.doc.bytes(), st.view.cursor) => {
            st.view.auto_indent_pending = false;
        }
        // Typing anything keeps the indent (nvim clears `did_ai` on a typed blank too).
        Command::InsertChar(_)
        | Command::InsertTab
        | Command::InsertPaste(_)
        | Command::InsertRegister(_)
        | Command::InsertEval(_)
        | Command::EnterNormal
        | Command::EnterInsert
        | Command::EnterInsertAfter
        | Command::InsertLineStart
        | Command::InsertColumnZero
        | Command::AppendLineEnd => st.view.auto_indent_pending = false,
        _ => {}
    }
    st.view.si_state = opened.si;
    match cmd {
        Command::InsertDeleteIndentRestore => st.view.old_indent = opened.deleted_indent,
        Command::InsertNewline if st.view.mode == Mode::Insert => st.view.old_indent = 0,
        _ => {}
    }
    st.view.end_comment = if opened.leader {
        opened.end_comment
    } else {
        None
    };
    if !st.view.auto_indent_pending {
        st.view.end_comment = None;
    }
    st.view.typed_space = match cmd {
        Command::InsertChar(' ') => true,
        Command::InsertChar(_) | Command::DeleteBack | Command::SetIndentCols { .. } => {
            st.view.typed_space
        }
        _ => false,
    };
}

/// Whether the cursor left its line: `line_start(after, cur) != line_start(before, entry)`. On an unchanged
/// buffer that is a `\n` between the two positions, which costs the distance moved, not the line (#1231).
fn left_line(before: &[u8], entry: usize, after: &[u8], cur: usize) -> bool {
    // `before` is held for the whole command, so an equal data pointer is the same, unchanged buffer.
    if std::ptr::eq(before.as_ptr(), after.as_ptr()) && before.len() == after.len() {
        let (lo, hi) = (
            entry.min(cur).min(after.len()),
            entry.max(cur).min(after.len()),
        );
        after[lo..hi].contains(&b'\n')
    } else {
        line_start(after, cur) != line_start(before, entry)
    }
}

/// Whether the caret sits after nothing but a non-empty indent on its line (an autoindent's leftover).
fn only_indent_before_caret(st: &EditorState) -> bool {
    let b = st.doc.bytes();
    let cur = st.view.cursor;
    let ls = line_start(b, cur);
    cur > ls && b[ls..cur].iter().all(|&c| c == b' ' || c == b'\t')
}

/// The `'smartindent'` rules a line opened by `cmd` arms (#617): computed from the line it opens from,
/// before the edit. Every other command disarms them (nvim clears them on the first typed key).
fn opened_si_state(st: &EditorState, cmd: &Command) -> indent::SiState {
    let si = st.view.indent.smartindent && st.view.caret == CaretGravity::OnChar;
    let b = st.doc.bytes();
    let cur = st.view.cursor;
    let ls = || line_start(b, cur); // only an open needs the line (#1231)
    match cmd {
        Command::OpenBelow | Command::OpenAbove if si => {
            let below = matches!(cmd, Command::OpenBelow);
            let words = &st.view.format.cinwords;
            st.view
                .indent
                .open_indent(b, ls(), line_end(b, cur), below, words)
                .1
        }
        Command::InsertNewline if si && st.view.mode == Mode::Insert => {
            st.view
                .indent
                .open_indent(b, ls(), cur, true, &st.view.format.cinwords)
                .1
        }
        _ => indent::SiState::default(),
    }
}

/// Maintain Vim's `Insstart` column ([`View::insert_col`]) after a command: set at Insert entry; inside
/// Insert a caret jump arms a restart and the next edit moves it to where that edit began (Vim's
/// `stop_arrow`). `CTRL-G U` steps stay inside the run.
pub(super) fn track_insert_start(
    st: &mut EditorState,
    cmd: &Command,
    entry_mode: Mode,
    entry_cursor: usize,
    entry_rev: crate::Revision,
    entry_col: &dyn Fn() -> usize,
) {
    if !is_text_entry(st.view.mode) {
        st.view.insert_jumped = false;
        st.view.insstart = None;
        st.view.insstart_line = None;
        return;
    }
    if !is_text_entry(entry_mode) {
        let b = st.doc.bytes();
        st.view.insert_col = col_of(b, line_start(b, st.view.cursor), st.view.cursor);
        st.view.insstart = Some(st.view.cursor);
        let ls = line_start(b, st.view.cursor);
        st.view.insstart_line = Some((
            crate::pos::raw::line_of(b, st.view.cursor),
            range::line_cells(b, ls, st.view.indent.tabs()),
        ));
        st.view.insert_jumped = false;
    } else if st.doc.revision() != entry_rev {
        if std::mem::take(&mut st.view.insert_jumped) {
            st.view.insert_col = entry_col();
        }
        // A delete back past the start (`<BS>`, `CTRL-W`, `CTRL-U` with 'backspace' "start") lowers it.
        let cur = st.view.cursor;
        st.view.insstart = Some(st.view.insstart.map_or(cur, |s| s.min(cur)));
    } else if st.view.cursor != entry_cursor && !matches!(cmd, Command::InsertCaretStep { .. }) {
        st.view.insert_jumped = true;
    }
}
