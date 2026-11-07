//! Python indenting on the key path (#1205, #1206): `=`, `o`/`O`/`<CR>`, `cc`/`S` and the keys of nvim's
//! Python 'indentkeys' run [`crate::indent::python`] the way nvim runs `python#GetIndent`, after the edit
//! that triggers it. Other languages keep the node-depth indent of `resolve_tree_aware`, and the word and
//! character keys of their own 'indentkeys' (C, JavaScript, Go, sh, Rust, CSS:
//! [`crate::indent::key_reindent`])
//! re-indent the line by its depth in the tree parsed after the key.
//!
//! The frontend owns the tree, so it computes the indent and runs a [`Command::SetIndentCols`] after the
//! key's own command; the trace records both. A `.` repeat replays the typed keys without re-indenting,
//! as the node-depth indent's `o` does.

use std::collections::HashMap;

use ruse_core::pos::{line_end, line_of, line_start};
use ruse_core::{BytePos, CinKey, Command, DocumentId, EditorOption, Mode, Workspace};

use crate::highlight::CachedHighlight;
use crate::indent::python::{self, Buf, Opts};
use crate::indent::{IndentTree, KeyReindent};

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
    ["javascript", "go", "bash", "rust", "css"]
        .into_iter()
        .find(|&l| l == name)
}

/// Whether the focused buffer indents by the C rules (nvim's cindent, #1163): a C buffer (nvim's
/// `indent/c.vim` sets 'cindent'), or a buffer without an indenter of its own (no tree, or Markdown)
/// under `'cindent'`. `'lisp'` wins over both, as in nvim.
pub(crate) fn is_cindent(
    ws: &Workspace,
    highlighters: &HashMap<DocumentId, CachedHighlight>,
) -> bool {
    let view = ws.focused().view;
    if view.lisp() {
        return false;
    }
    match highlighters
        .get(&ws.focused_buffer())
        .and_then(CachedHighlight::tree)
    {
        Some(t) if t.language().name() == Some("c") => true,
        Some(t) if crate::indent::uses_node_depth(t) || crate::indent::is_python(t) => false,
        _ => view.cindent(),
    }
}

/// `i_CTRL-F` in a cindent buffer: the caret's line, re-indented by the C rules.
pub(crate) fn c_reindent_caret_line(ws: &Workspace, snapshot: &[u8]) -> Command {
    let view = ws.focused().view;
    let cur = view.cursor().0.min(snapshot.len());
    let row = line_of(snapshot, BytePos(cur));
    let col = cur - line_start(snapshot, BytePos(cur)).0;
    Command::SetIndentCols {
        first_line: row,
        last_line: row,
        cols: vec![view.c_indent(snapshot, row, col, true)],
    }
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
        tabstop: view.tabs(),
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
    /// A line was opened (`o`/`<CR>`, or `O` when `back`).
    Open {
        back: bool,
    },
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
    // Python and the C rules both re-indent after the key, as nvim's indent script and cindent do.
    let python =
        (is_python(ws, highlighters) && !ws.focused().view.lisp()) || is_cindent(ws, highlighters);
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
                After::Open { back: false }
            } else {
                After::Nothing
            }
        }
        _ if !python => After::Nothing,
        Command::OpenBelow if !insert => After::Open { back: false },
        Command::OpenAbove if !insert => After::Open { back: true },
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
    if is_cindent(ws, highlighters) {
        return c_key_reindent(after, ws);
    }
    if let Some(lang) = depth_keys_lang(ws, highlighters) {
        return depth_key_reindent(lang, after, ws, highlighters);
    }
    let wanted = match after {
        After::Nothing => false,
        After::Typed { ch, white } => python::reindents_on(ch, &b[ls..cur.0], white),
        After::Open { .. } => true,
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

/// The C rules after a key (#1163), as nvim runs them: a typed key of `'cinkeys'` (`insert_do_cindent`),
/// an opened line when `'cinkeys'` holds `o`/`O` (`open_line`), and a linewise change (`fix_indent`, no
/// key check) re-indent the caret's line by `get_c_indent`.
fn c_key_reindent(after: After, ws: &Workspace) -> Option<Command> {
    let pane = ws.focused();
    let view = pane.view;
    let b = pane.doc.bytes();
    let cur = view.cursor();
    let ls = line_start(b, cur).0;
    let le = line_end(b, cur).0;
    let row = line_of(b, BytePos(ls));
    let col = cur.0 - ls;
    let blank = b[ls..le].iter().all(|&c| c == b' ' || c == b'\t');
    let wanted = match after {
        After::Nothing => false,
        // A key is one byte in 'cinkeys'; a multi-byte char never matches one.
        After::Typed { ch, white } => u8::try_from(ch)
            .ok()
            .filter(u8::is_ascii)
            .is_some_and(|k| view.c_key_reindents(b, row, col, CinKey::Char(k), white)),
        After::Open { back } => {
            let key = if back {
                CinKey::OpenBack
            } else {
                CinKey::OpenForw
            };
            view.c_key_reindents(b, row, col, key, blank)
        }
        After::Change => ws.registers().unnamed().is_linewise() && blank,
    };
    if !wanted {
        return None;
    }
    let cols = view.c_indent(b, row, col, true)?;
    set_line_indent(ws, row, cols)
}

/// A typed key of a node-depth language's 'indentkeys' (#1206): the line takes its depth in the tree
/// parsed after the key, or column 0 for C's `#` and jump labels. Where the tree is broken around the line
/// even with its open blocks closed, it takes the indent of the line above, as autoindent gave it
/// (#1214): typing on past a keyword (`fi` to `find`) undoes the keyword's re-indent. A `{` that opens
/// the body of a statement above lines up with that statement's line, in the columns it has.
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
    let ts = pane.view.tabs();
    let sw = pane.view.shift_width();
    // The cached tree is only shifted by the key's edit; the line's window is parsed afresh.
    let like = highlighters
        .get(&ws.focused_buffer())
        .and_then(CachedHighlight::tree);
    let cols = match key {
        KeyReindent::GoCase => go_case_indent(b, ls, le, ts, sw),
        KeyReindent::Brace => {
            let tree = key_window(like?, b, row, ls, le)?;
            match tree.brace_align(cur.0 - 1)? {
                Some(header) => line_indent(b, header, ts),
                None => tree.levels(row, row)[0] * sw,
            }
        }
        KeyReindent::Depth => {
            let level = |bytes: &[u8], le: usize| {
                key_window(like?, bytes, row, ls, le).map(|t| t.levels(row, row)[0])
            };
            // A keyword that still waits for its statement (`else`, `elif`) breaks the tree by itself:
            // ask again with a statement filled in after the caret.
            let fill = crate::indent::key_placeholder(lang, b[ls..cur.0].trim_ascii_start());
            let level = level(b, le).or_else(|| {
                (!fill.is_empty())
                    .then(|| level(&[&b[..cur.0], fill, &b[cur.0..]].concat(), le + fill.len()))
                    .flatten()
            });
            level.map_or_else(|| prev_line_indent(b, ls, ts), |l| l * sw)
        }
    };
    set_line_indent(ws, row, cols)
}

/// The window tree for the line `row` (starting at `ls`, ending at `le`) of `bytes`, in the language of
/// `like`, unless it is broken at the line's end even with its open blocks closed.
fn key_window<'a>(
    like: &tree_sitter::Tree,
    bytes: &'a [u8],
    row: usize,
    ls: usize,
    le: usize,
) -> Option<IndentTree<'a>> {
    IndentTree::window(like, bytes, row, ls, le).filter(|tree| !tree.broken_at(le))
}

/// nvim's `GoIndent` (`indent/go.vim`) for the line `[ls, le)`, as its `<:>` key runs it on a
/// `case`/`default` label (#1206): the indent of the last non-blank line above, one `sw` more when that
/// line ends in `(` or `{` and one more when it is a label, one less when this line starts with a closer
/// and one less for the label itself. `//` comments are dropped first. Saturates at column 0.
fn go_case_indent(
    b: &[u8],
    ls: usize,
    le: usize,
    ts: impl Into<ruse_core::Tabs>,
    sw: usize,
) -> usize {
    let ts: ruse_core::Tabs = ts.into();
    let strip = |l: &[u8]| -> Vec<u8> {
        let end = l.windows(2).position(|w| w == b"//").unwrap_or(l.len());
        l[..end].to_vec()
    };
    let is_label = |l: &[u8]| {
        let t = l.trim_ascii_start();
        t.ends_with(b":") && (t.starts_with(b"case ") || t == b"default:")
    };
    let mut end = ls;
    let mut prev = None;
    while end > 0 {
        let start = b[..end - 1]
            .iter()
            .rposition(|&c| c == b'\n')
            .map_or(0, |i| i + 1);
        let line = &b[start..end - 1];
        if !line.iter().all(|&c| c.is_ascii_whitespace()) {
            prev = Some(line);
            break;
        }
        end = start;
    }
    let Some(prev) = prev else {
        return 0;
    };
    let this = strip(&b[ls..le]);
    let prevl = strip(prev);
    let mut ind = indent_cols(prev, ts) as isize;
    let sw = sw as isize;
    if matches!(prevl.trim_ascii_end().last(), Some(b'(' | b'{')) {
        ind += sw;
    }
    if is_label(&prevl) {
        ind += sw;
    }
    if matches!(this.trim_ascii_start().first(), Some(b')' | b'}')) {
        ind -= sw;
    }
    if is_label(&this) {
        ind -= sw;
    }
    ind.max(0) as usize
}

/// The indent in columns of buffer row `row`.
fn line_indent(b: &[u8], row: usize, ts: impl Into<ruse_core::Tabs>) -> usize {
    let ts: ruse_core::Tabs = ts.into();
    let mut ls = 0;
    for _ in 0..row {
        match b[ls..].iter().position(|&c| c == b'\n') {
            Some(i) => ls += i + 1,
            None => return 0,
        }
    }
    indent_cols(&b[ls..], ts)
}

/// The indent in columns of the last non-blank line above the line starting at `ls`, or 0.
fn prev_line_indent(b: &[u8], ls: usize, ts: impl Into<ruse_core::Tabs>) -> usize {
    let ts: ruse_core::Tabs = ts.into();
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
fn indent_cols(line: &[u8], ts: impl Into<ruse_core::Tabs>) -> usize {
    let ts: ruse_core::Tabs = ts.into();
    line.iter()
        .take_while(|&&c| c == b' ' || c == b'\t')
        .fold(
            0,
            |c, &x| if x == b'\t' { c + ts.padding(c) } else { c + 1 },
        )
}

/// A [`Command::SetIndentCols`] that gives line `row` of the focused buffer `cols` columns of indent, or
/// `None` when it has them already.
fn set_line_indent(ws: &Workspace, row: usize, cols: usize) -> Option<Command> {
    let pane = ws.focused();
    let b = pane.doc.bytes();
    let cur = pane.view.cursor();
    let line = &b[line_start(b, cur).0..line_end(b, cur).0];
    (cols != indent_cols(line, pane.view.tabs())).then(|| Command::SetIndentCols {
        first_line: row,
        last_line: row,
        cols: vec![Some(cols)],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #1206: Go's `case` key reads the line above, as `GoIndent` does.
    #[test]
    fn go_case_reads_the_line_above() {
        let at = |src: &[u8]| {
            let ls = src.iter().rposition(|&c| c == b'\n').map_or(0, |i| i + 1);
            go_case_indent(src, ls, src.len(), 8, 8)
        };
        assert_eq!(at(b"\tswitch a {\n\tcase 1:"), 8);
        assert_eq!(at(b"\tcase 1:\n\t\tx()\n\tcase 2:"), 8);
        assert_eq!(at(b"\t\tcase 1:\n\t\t\tx()\n\n\tcase 2:"), 16);
        assert_eq!(at(b"\tcase 1:\n\tdefault:"), 8);
        assert_eq!(at(b"\tx() // {\n\tcase 1:"), 0);
        assert_eq!(at(b"case 1:"), 0);
    }

    #[test]
    fn line_indent_reads_a_row() {
        assert_eq!(line_indent(b"a\n    b\n\tc", 1, 8), 4);
        assert_eq!(line_indent(b"a\n    b\n\tc", 2, 8), 8);
        assert_eq!(line_indent(b"a", 3, 8), 0);
    }
}
