//! Python indenting on the key path (#1205, #1206): `=`, `o`/`O`/`<CR>`, `cc`/`S` and the keys of nvim's
//! Python 'indentkeys' run [`crate::indent::python`] the way nvim runs `python#GetIndent`, after the edit
//! that triggers it. Other languages keep the node-depth indent of `resolve_tree_aware`, and the word and
//! character keys of their own 'indentkeys' (C, JavaScript, Go, sh: [`crate::indent::key_reindent`])
//! re-indent the line by its depth in the tree parsed after the key.
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

/// The language of the focused buffer's tree when it is indented by node depth and has 'indentkeys' of
/// its own ([`crate::indent::key_reindent`], #1206): not Python, Markdown or a `'lisp'` buffer.
fn depth_keys_lang(
    ws: &Workspace,
    highlighters: &HashMap<DocumentId, CachedHighlight>,
) -> Option<&'static str> {
    if ws.focused().view.lisp() {
        return None;
    }
    let tree = highlighters
        .get(&ws.focused_buffer())
        .and_then(CachedHighlight::tree)?;
    let lang = tree.language();
    let name = lang.name()?;
    ["c", "javascript", "go", "bash"]
        .into_iter()
        .find(|&l| l == name)
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
    let python = is_python(ws, highlighters) && !ws.focused().view.lisp();
    if !python && depth_keys_lang(ws, highlighters).is_none() {
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
        // The other languages indent an opened or changed line in `resolve_tree_aware`.
        Command::InsertNewline if insert => {
            *can = true;
            if python {
                After::Open
            } else {
                After::Nothing
            }
        }
        _ if !python => After::Nothing,
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
    if let Some(lang) = depth_keys_lang(ws, highlighters) {
        return depth_key_reindent(lang, after, ws, highlighters);
    }
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
    set_line_indent(ws, row, cols)
}

/// A typed key of a node-depth language's 'indentkeys' (#1206): the line takes its depth in the tree
/// parsed after the key, or column 0 for C's `#`. Where the tree is broken around the line even with its
/// open blocks closed, it takes the indent of the line above, as autoindent gave it (#1214): typing on
/// past a keyword (`fi` to `find`) undoes the keyword's re-indent.
fn depth_key_reindent(
    lang: &str,
    after: After,
    ws: &Workspace,
    highlighters: &HashMap<DocumentId, CachedHighlight>,
) -> Option<Command> {
    let After::Typed { ch, white } = after else {
        return None;
    };
    let pane = ws.focused();
    let b = pane.doc.bytes();
    let cur = pane.view.cursor();
    let ls = line_start(b, cur).0;
    let le = line_end(b, cur).0;
    let key = crate::indent::key_reindent(lang, ch, &b[ls..cur.0], white)?;
    let row = line_of(b, BytePos(ls));
    let cols = match key {
        crate::indent::KeyReindent::Column0 => 0,
        crate::indent::KeyReindent::Depth => {
            // The cached tree is only shifted by the key's edit; the line's window is parsed afresh.
            let like = highlighters
                .get(&ws.focused_buffer())
                .and_then(CachedHighlight::tree)?;
            let level = |bytes: &[u8], le: usize| {
                let tree = crate::indent::IndentTree::window(like, bytes, row, ls, le)?;
                (!tree.broken_at(le)).then(|| tree.levels(row, row)[0])
            };
            // A keyword that still waits for its statement (`else`, `elif`) breaks the tree by itself:
            // ask again with a statement filled in after the caret.
            let fill = crate::indent::key_placeholder(lang, b[ls..cur.0].trim_ascii_start());
            let level = level(b, le).or_else(|| {
                (!fill.is_empty())
                    .then(|| level(&[&b[..cur.0], fill, &b[cur.0..]].concat(), le + fill.len()))
                    .flatten()
            });
            level.map_or_else(
                || prev_line_indent(b, ls, pane.view.tab_width()),
                |l| l * pane.view.shift_width(),
            )
        }
    };
    set_line_indent(ws, row, cols)
}

/// The indent in columns of the last non-blank line above the line starting at `ls`, or 0.
fn prev_line_indent(b: &[u8], ls: usize, ts: usize) -> usize {
    let mut end = ls;
    while end > 0 {
        let start = b[..end - 1]
            .iter()
            .rposition(|&c| c == b'\n')
            .map_or(0, |i| i + 1);
        let line = &b[start..end - 1];
        if !line.iter().all(|&c| c.is_ascii_whitespace()) {
            return indent_cols(line, ts);
        }
        end = start;
    }
    0
}

/// The columns the leading blanks of `line` take.
fn indent_cols(line: &[u8], ts: usize) -> usize {
    let ts = ts.max(1);
    line.iter()
        .take_while(|&&c| c == b' ' || c == b'\t')
        .fold(0, |c, &x| if x == b'\t' { c + ts - c % ts } else { c + 1 })
}

/// A [`Command::SetIndentCols`] that gives line `row` of the focused buffer `cols` columns of indent, or
/// `None` when it has them already.
fn set_line_indent(ws: &Workspace, row: usize, cols: usize) -> Option<Command> {
    let pane = ws.focused();
    let b = pane.doc.bytes();
    let cur = pane.view.cursor();
    let line = &b[line_start(b, cur).0..line_end(b, cur).0];
    (cols != indent_cols(line, pane.view.tab_width())).then(|| Command::SetIndentCols {
        first_line: row,
        last_line: row,
        cols: vec![Some(cols)],
    })
}
