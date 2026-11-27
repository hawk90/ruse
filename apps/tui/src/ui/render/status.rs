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

/// The bar's prefix: the mode, the focused buffer's name and flags, the window position and the diagnostic
/// counts, then the two blanks a message at `'cmdheight'` 0 follows.
pub(super) fn bar_prefix(ws: &Workspace, s: &StatusInput<'_>) -> String {
    let focus = ws.focused();
    let StatusInput {
        flags,
        diagnostics,
        showmode,
        ..
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
    if showmode {
        format!("{mode}  {name}{dirty}{win}{diag}  ")
    } else {
        format!("{name}{dirty}{win}{diag}  ")
    }
}

/// The bar at `'cmdheight'` N ≥ 1: its prefix alone, the message has rows of its own.
fn bar_alone(prefix: &str) -> &str {
    prefix.strip_suffix("  ").unwrap_or(prefix)
}

/// The status line's text at `'cmdheight'` 0, when no command line is open: the bar's `prefix`, then the
/// message. A message taller than the screen is paged into `cur` above it with `-- More --` (#1235).
fn status_text(
    cur: &mut screen::Screen,
    prefix: &str,
    s: &StatusInput<'_>,
    pager: &mut crate::ui::message::Pager,
    cols: u16,
    text_rows: u16,
) -> String {
    let StatusInput {
        status,
        question,
        status_serial,
        ..
    } = *s;
    // A message too long for the rest of the row is shown whole above a hit-enter prompt, or a
    // file message cut at its start (#1222). One taller than the screen is paged from its top
    // with `-- More --` (#1235).
    use crate::ui::message::{fit, fit_question, Shown};
    let shown = if question {
        fit_question(status, usize::from(cols))
    } else {
        fit(crate::ui::message::width(prefix), status, usize::from(cols))
    };
    match shown {
        Shown::Fits => format!("{prefix}{status}"),
        Shown::Cut(tail) => format!("{prefix}{tail}"),
        Shown::CutRow(tail) => tail,
        Shown::More(lines) => {
            paint_paged(cur, &lines, status_serial, pager, cols, text_rows, false)
        }
    }
}

/// Paint `lines` over the `page` rows above the prompt on the last row, paged from the pager's top, and
/// give the prompt's text. With `gap`, a message shorter than the page has a blank row above it, as nvim
/// shows a message that grows from its message rows (v0.12.6, `:echo "a\nb\nc"` in a PTY at 80x24).
fn paint_paged(
    cur: &mut screen::Screen,
    lines: &[String],
    serial: u64,
    pager: &mut crate::ui::message::Pager,
    cols: u16,
    page: u16,
    gap: bool,
) -> String {
    let (first, prompt) = pager.layout(serial, lines.len(), usize::from(page));
    let shown = &lines[first..lines.len().min(first + usize::from(page))];
    let top = page - shown.len() as u16;
    if gap && top > 0 {
        paint_row(cur, top - 1, cols, 0, "", false);
    }
    for (i, line) in shown.iter().enumerate() {
        paint_row(cur, top + i as u16, cols, 0, line, false);
    }
    pager.prompt_text(prompt)
}

/// At `'cmdheight'` N ≥ 1, paint the bar alone on its own row (the menus then go over it).
pub(super) fn paint_bar(
    cur: &mut screen::Screen,
    prefix: &str,
    bottom: crate::ui::layout::BottomLayout,
) {
    if bottom.msg_rows > 0 {
        let bar = bar_alone(prefix);
        cur.put_str(
            bottom.bar_row,
            0,
            bar,
            crossterm::style::Color::Reset,
            false,
        );
    }
}

/// Paint the rows below the windows (RFC-0027 §1.1) but the bar of N ≥ 1, which goes under the menus: at
/// `'cmdheight'` 0 the bar and its message, or the command line `cmd` over them; at N ≥ 1 the command line
/// on the first message row, or the message. Gives the text of the last row, where a prompt waits.
pub(super) fn paint_bottom(
    cur: &mut screen::Screen,
    cmd: Option<&str>,
    prefix: &str,
    s: &StatusInput<'_>,
    pager: &mut crate::ui::message::Pager,
    bottom: crate::ui::layout::BottomLayout,
    (cols, rows): (u16, u16),
) -> String {
    use crossterm::style::Color;
    pager.prompt = crate::ui::message::Prompt::None;
    if bottom.msg_rows == 0 {
        let bar = match cmd {
            Some(shown) => shown.to_string(),
            None => status_text(cur, prefix, s, pager, cols, bottom.text_rows),
        };
        // put_str truncates at the right edge.
        cur.put_str(bottom.bar_row, 0, &bar, Color::Reset, false);
        return bar;
    }
    if let Some(shown) = cmd {
        cur.put_str(bottom.cmd_row(), 0, shown, Color::Reset, false);
        return String::new();
    }
    match message_rows(cur, s, pager, bottom, cols, rows) {
        Some(prompt) => {
            cur.put_str(rows.saturating_sub(1), 0, &prompt, Color::Reset, false);
            prompt
        }
        None => String::new(),
    }
}

/// The message rows at `'cmdheight'` N ≥ 1 (RFC-0027 §1.2), when no command line is open. A message that
/// takes at most N rows is painted on them from the first; a question keeps the first row whole (#619). Any
/// other grows up over the bar and the windows to the hit-enter prompt (or `-- More --`) on the last row,
/// whose text this gives.
fn message_rows(
    cur: &mut screen::Screen,
    s: &StatusInput<'_>,
    pager: &mut crate::ui::message::Pager,
    layout: crate::ui::layout::BottomLayout,
    cols: u16,
    rows: u16,
) -> Option<String> {
    use crate::ui::message::{fit_question, fit_rows, Rows, Shown};
    if s.status.is_empty() {
        return None;
    }
    let first = layout.cmd_row();
    let shown = if s.question {
        match fit_question(s.status, usize::from(cols)) {
            Shown::CutRow(row) => Rows::Fit(vec![row]),
            _ => unreachable!("a question is cut to its row"),
        }
    } else {
        fit_rows(s.status, usize::from(cols), usize::from(layout.msg_rows))
    };
    match shown {
        Rows::Fit(lines) => {
            for (i, line) in lines.iter().take(usize::from(layout.msg_rows)).enumerate() {
                paint_row(cur, first + i as u16, cols, 0, line, false);
            }
            None
        }
        Rows::More(lines) => {
            let page = rows.saturating_sub(1);
            Some(paint_paged(
                cur,
                &lines,
                s.status_serial,
                pager,
                cols,
                page,
                true,
            ))
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
