//! The status line: the mode, the buffer, its flags and the message, paged when it is taller than the
//! screen. Split from `render()` (#1399).

use super::{paint_row, screen, Mode, SelectKind, Workspace};

/// What the status line reads besides the workspace.
#[derive(Clone, Copy)]
pub(super) struct StatusInput<'a> {
    pub(super) status: &'a str,
    /// The `%r`-style flags after `[+]`.
    pub(super) flags: &'a str,
    pub(super) diagnostics: &'a [crate::lsp::Diag],
    pub(super) showmode: bool,
    /// The message is a question (it keeps its whole row).
    pub(super) question: bool,
    pub(super) status_serial: u64,
}

/// The status line's text when no command line is open: the mode, the focused buffer's name and flags,
/// the window position, the diagnostic counts, then the message. A message taller than the screen is paged
/// into `cur` above it with `-- More --` (#1235).
pub(super) fn status_text(
    cur: &mut screen::Screen,
    ws: &Workspace,
    s: &StatusInput<'_>,
    pager: &mut crate::ui::message::Pager,
    cols: u16,
    text_rows: u16,
) -> String {
    let focus = ws.focused();
    let StatusInput {
        status,
        flags,
        diagnostics,
        showmode,
        question,
        status_serial,
    } = *s;
    let mode = mode_name(focus.view.mode());
    // Show the FOCUSED buffer's name (multi-buffer, F-007), not the session path — so switching to
    // a scratch buffer reads `[No Name]`, not the file that `path` still points at.
    let name = ws
        .buffer_name(ws.focused_buffer())
        .map_or_else(|| "[No Name]".to_string(), str::to_string);
    // nvim's `%m%r` after the name: `f.txt [+][RO]`; `flags` holds the ones after `[+]`.
    let modified = if focus.doc.is_modified() { "[+]" } else { "" };
    let dirty = if modified.is_empty() && flags.is_empty() {
        String::new()
    } else {
        format!(" {modified}{flags}")
    };
    // Show the window position only once split, so the single-window status line is unchanged.
    let win = if ws.window_count() > 1 {
        format!("  [win {}/{}]", ws.focus() + 1, ws.window_count())
    } else {
        String::new()
    };
    // F-014: a diagnostic summary for the focused buffer, when any (errors/warnings).
    let diag = match crate::lsp::counts(diagnostics) {
        (0, 0) => String::new(),
        (e, w) => format!("  [E:{e} W:{w}]"),
    };
    // `'noshowmode'` leaves the mode out (#1151).
    let prefix = if showmode {
        format!("{mode}  {name}{dirty}{win}{diag}  ")
    } else {
        format!("{name}{dirty}{win}{diag}  ")
    };
    // A message too long for the rest of the row is shown whole above a hit-enter prompt, or a
    // file message cut at its start (#1222). One taller than the screen is paged from its top
    // with `-- More --` (#1235).
    use crate::ui::message::{fit, fit_question, Shown};
    let shown = if question {
        fit_question(status, usize::from(cols))
    } else {
        fit(
            crate::ui::message::width(&prefix),
            status,
            usize::from(cols),
        )
    };
    match shown {
        Shown::Fits => format!("{prefix}{status}"),
        Shown::Cut(tail) => format!("{prefix}{tail}"),
        Shown::CutRow(tail) => tail,
        Shown::More(lines) => {
            let page = usize::from(text_rows);
            let (first, prompt) = pager.layout(status_serial, lines.len(), page);
            let shown = &lines[first..lines.len().min(first + page)];
            let top = text_rows - shown.len() as u16;
            for (i, line) in shown.iter().enumerate() {
                let row = top + i as u16;
                paint_row(cur, row, cols, 0, line, false);
            }
            pager.prompt_text(prompt)
        }
    }
}

/// The mode name the status line shows.
fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Normal => "NORMAL",
        Mode::Insert => "INSERT",
        Mode::Replace => "REPLACE",
        Mode::VirtualReplace => "V-REPLACE",
        Mode::Visual {
            kind: SelectKind::Charwise,
        } => "VISUAL",
        Mode::Visual {
            kind: SelectKind::Linewise,
        } => "V-LINE",
        Mode::Visual {
            kind: SelectKind::Blockwise,
        } => "V-BLOCK",
        Mode::Select {
            kind: SelectKind::Charwise,
        } => "SELECT",
        Mode::Select {
            kind: SelectKind::Linewise,
        } => "S-LINE",
        Mode::Select {
            kind: SelectKind::Blockwise,
        } => "S-BLOCK",
        Mode::Terminal => "TERMINAL",
        Mode::TerminalNormal => "T-NORMAL",
    }
}
