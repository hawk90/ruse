//! Python indenting on the key path (#1205, #1206): `=`, `o`/`O`/`<CR>`, `cc`/`S` and the keys of nvim's
//! Python 'indentkeys' run [`crate::indent::python`] the way nvim runs `python#GetIndent`, after the edit
//! that triggers it. Other languages keep the node-depth indent of `resolve_tree_aware`.
//!
//! The frontend owns the tree, so it computes the indent and runs a [`Command::SetIndentCols`] after the
//! key's own command; the trace records both. A `.` repeat replays the typed keys without re-indenting,
//! as the node-depth indent's `o` does.

use std::collections::HashMap;

use ruse_core::pos::{line_end, line_of, line_start};
use ruse_core::{BytePos, Command, DocumentId, EditorOption, Mode, Workspace};

use crate::highlight::CachedHighlight;
use crate::indent::python::{self, Buf, Opts};

/// The focused buffer's tree lined up with `bytes`, when the buffer is Python.
fn python_tree(
    ws: &Workspace,
    highlighters: &HashMap<DocumentId, CachedHighlight>,
    bytes: &[u8],
) -> Option<tree_sitter::Tree> {
    let hl = highlighters.get(&ws.focused_buffer())?;
    hl.tree()
        .is_some_and(crate::indent::is_python)
        .then(|| hl.tree_matching(bytes))
        .flatten()
}

/// Whether the focused buffer is indented by the Python script.
pub(crate) fn is_python(
    ws: &Workspace,
    highlighters: &HashMap<DocumentId, CachedHighlight>,
) -> bool {
    highlighters
        .get(&ws.focused_buffer())
        .and_then(CachedHighlight::tree)
        .is_some_and(crate::indent::is_python)
}

/// The focused view's indent settings, as the script reads them.
fn opts(ws: &Workspace) -> Opts {
    let view = ws.focused().view;
    Opts {
        shiftwidth: view.shift_width(),
        tabstop: view.tab_width(),
        expandtab: view
            .editing_options()
            .iter()
            .any(|o| matches!(o, EditorOption::ExpandTab(true))),
    }
}

/// `=` over lines `[first, last]` of the Python buffer `snapshot`.
pub(crate) fn reindent(
    ws: &Workspace,
    highlighters: &HashMap<DocumentId, CachedHighlight>,
    snapshot: &[u8],
    first: usize,
    last: usize,
) -> Command {
    let tree = python_tree(ws, highlighters, snapshot);
    let mut buf = Buf::new(snapshot, tree.as_ref(), opts(ws));
    Command::SetIndentCols {
        first_line: first,
        last_line: last,
        cols: buf.reindent(first, last),
    }
}

/// `i_CTRL-F` in the Python buffer `snapshot`: the caret's line, re-indented.
pub(crate) fn reindent_caret_line(
    ws: &Workspace,
    highlighters: &HashMap<DocumentId, CachedHighlight>,
    snapshot: &[u8],
) -> Command {
    let row = line_of(snapshot, ws.focused().view.cursor());
    let tree = python_tree(ws, highlighters, snapshot);
    let buf = Buf::new(snapshot, tree.as_ref(), opts(ws));
    Command::SetIndentCols {
        first_line: row,
        last_line: row,
        cols: vec![buf.get_indent(row)],
    }
}

/// What to re-indent once a command has run, decided before it runs.
pub(crate) enum After {
    Nothing,
    /// A typed character that may be one of the 'indentkeys' (`white`: only blanks before the caret).
    Typed {
        ch: char,
        white: bool,
    },
    /// A line was opened (`o`/`O`/`<CR>`).
    Open,
    /// A change that may be linewise (`cc`, `S`, `cj`, `Vc`), entered from outside Insert.
    Change,
}

/// Read a command before it runs: what it re-indents afterwards, and what it does to nvim's
/// `can_cindent` (`can`), which a blank typed in the indent, a `<Tab>` or `<BS>` there clears until the
/// next line or Insert.
pub(crate) fn before(
    cmd: &Command,
    ws: &Workspace,
    highlighters: &HashMap<DocumentId, CachedHighlight>,
    can: &mut bool,
) -> After {
    if !is_python(ws, highlighters) {
        return After::Nothing;
    }
    let pane = ws.focused();
    let insert = pane.view.mode() == Mode::Insert;
    if !insert {
        *can = true;
    }
    let b = pane.doc.bytes();
    let cur = pane.view.cursor();
    let ls = line_start(b, cur).0;
    let in_indent = b[ls..cur.0].iter().all(|&c| c == b' ' || c == b'\t');
    match cmd {
        Command::InsertChar(' ') | Command::InsertTab | Command::DeleteBack
            if insert && in_indent =>
        {
            *can = false;
            After::Nothing
        }
        // `CTRL-T`/`CTRL-D` indent by hand (nvim's `ins_shift`).
        Command::InsertIndent
        | Command::InsertDedent
        | Command::InsertDeleteIndent
        | Command::InsertDeleteIndentRestore
            if insert =>
        {
            *can = false;
            After::Nothing
        }
        Command::InsertChar(ch) if insert && *can => After::Typed {
            ch: *ch,
            white: in_indent,
        },
        Command::InsertNewline if insert => {
            *can = true;
            After::Open
        }
        Command::OpenBelow | Command::OpenAbove if !insert => After::Open,
        Command::Change(..)
        | Command::OpForced { .. }
        | Command::ChangeSelection
        | Command::Search { .. }
            if !insert =>
        {
            After::Change
        }
        _ => After::Nothing,
    }
}

/// The re-indent a command calls for once it has run (see [`before`]), if the indent changes.
pub(crate) fn after(
    after: After,
    ws: &Workspace,
    highlighters: &HashMap<DocumentId, CachedHighlight>,
) -> Option<Command> {
    let pane = ws.focused();
    if pane.view.mode() != Mode::Insert {
        return None;
    }
    let b = pane.doc.bytes();
    let cur = pane.view.cursor();
    let ls = line_start(b, cur).0;
    let le = line_end(b, cur).0;
    let wanted = match after {
        After::Nothing => false,
        After::Typed { ch, white } => python::reindents_on(ch, &b[ls..cur.0], white),
        After::Open => true,
        // Linewise: the unnamed register took whole lines and the line is left blank.
        After::Change => {
            ws.registers().unnamed().is_linewise()
                && b[ls..le].iter().all(|&c| c == b' ' || c == b'\t')
        }
    };
    if !wanted {
        return None;
    }
    let row = line_of(b, BytePos(ls));
    let tree = python_tree(ws, highlighters, b);
    let buf = Buf::new(b, tree.as_ref(), opts(ws));
    let cols = buf.get_indent(row)?;
    let ws_end = ls
        + b[ls..le]
            .iter()
            .take_while(|&&c| c == b' ' || c == b'\t')
            .count();
    let now = b[ls..ws_end].iter().fold(0, |c, &x| {
        if x == b'\t' {
            let ts = pane.view.tab_width().max(1);
            c + ts - c % ts
        } else {
            c + 1
        }
    });
    (cols != now).then(|| Command::SetIndentCols {
        first_line: row,
        last_line: row,
        cols: vec![Some(cols)],
    })
}
