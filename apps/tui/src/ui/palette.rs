//! The command palette overlay (F-004 #2): a context-filtered, query-narrowed list of commands that
//! Enter dispatches by stable id through the normal command path.

use std::path::PathBuf;

use crossterm::event::KeyCode;

use ruse_core::{Command, Workspace};

use crate::app::dispatch::run_cmd;
use crate::persist;

/// The command-palette overlay state (F-004 #2): the commands AVAILABLE in the current context, the
/// query filtering them, and the selected row. Opened with a dedicated key; Enter dispatches the
/// selected command by its stable id (never a key), Esc closes.
pub(crate) struct Palette {
    /// The typed filter.
    pub(crate) query: String,
    /// Commands available in the opening context (before the query filter).
    available: Vec<ruse_core::CommandSpec>,
    /// The current query's matches (a subset of `available`).
    pub(crate) matches: Vec<ruse_core::CommandSpec>,
    /// Selected row into `matches`.
    pub(crate) selected: usize,
}

impl Palette {
    pub(crate) fn open(ctx: &ruse_core::Context) -> Palette {
        let mut p = Palette {
            query: String::new(),
            available: ruse_core::available(ctx),
            matches: Vec::new(),
            selected: 0,
        };
        p.refilter();
        p
    }

    /// Recompute `matches` from `available` and the query (case-insensitive substring on title or id).
    fn refilter(&mut self) {
        let q = self.query.to_lowercase();
        self.matches = self
            .available
            .iter()
            .filter(|s| q.is_empty() || s.title.to_lowercase().contains(&q) || s.id.contains(&q))
            .cloned()
            .collect();
        self.selected = self.selected.min(self.matches.len().saturating_sub(1));
    }

    fn selected_command(&self) -> Option<ruse_core::Command> {
        self.matches.get(self.selected).map(|s| s.command.clone())
    }
}

/// The command-availability context of the focused view (F-004 #2 C-CONTEXT).
pub(crate) fn focused_context(ws: &Workspace) -> ruse_core::Context {
    let f = ws.focused();
    let bytes = f.doc.bytes();
    let has_selection =
        f.view.selection_span(bytes).is_some() || f.view.block_spans(bytes).is_some();
    ruse_core::Context {
        mode: f.view.mode(),
        has_selection,
    }
}

/// Handle one key of the command palette (F-004 #2). Enter dispatches the selected command by its id
/// (through the normal command path, so it undoes/records like any other); Esc closes.
#[allow(clippy::too_many_arguments)]
pub(crate) fn palette_key(
    palette: &mut Option<Palette>,
    key: crossterm::event::KeyEvent,
    ws: &mut Workspace,
    path: &Option<PathBuf>,
    fmt: persist::encoding::FileFormat,
    file_buf: ruse_core::DocumentId,
    recorded: &mut Vec<Command>,
    status: &mut String,
    quit: &mut bool,
) {
    let Some(p) = palette.as_mut() else {
        return;
    };
    match key.code {
        KeyCode::Esc => *palette = None,
        KeyCode::Enter => {
            let cmd = p.selected_command();
            *palette = None;
            if let Some(cmd) = cmd {
                run_cmd(cmd, ws, path, fmt, file_buf, recorded, status, quit);
            }
        }
        KeyCode::Up => p.selected = p.selected.saturating_sub(1),
        KeyCode::Down if p.selected + 1 < p.matches.len() => p.selected += 1,
        KeyCode::Backspace => {
            p.query.pop();
            p.refilter();
        }
        KeyCode::Char(c) => {
            p.query.push(c);
            p.refilter();
        }
        _ => {}
    }
}

#[cfg(test)]
mod palette_tests {
    use super::*;
    use ruse_core::{Context, Mode};

    fn normal_ctx() -> Context {
        Context {
            mode: Mode::Normal,
            has_selection: false,
        }
    }

    /// F-004 #2: the palette opens context-filtered and the query narrows it further; Enter yields the
    /// selected command (by id, decoupled from any key).
    #[test]
    fn palette_filters_by_context_then_query() {
        let mut p = Palette::open(&normal_ctx());
        let opened: Vec<_> = p.matches.iter().map(|s| s.id).collect();
        assert!(
            opened.contains(&"editor.undo"),
            "Normal-family command is offered"
        );
        assert!(
            !opened.contains(&"editor.delete_back"),
            "Insert-only command is hidden in Normal"
        );

        for c in "save".chars() {
            p.query.push(c);
            p.refilter();
        }
        assert_eq!(
            p.matches.len(),
            1,
            "query narrows to the single 'Save File' match"
        );
        assert_eq!(p.selected_command(), Some(ruse_core::Command::Save));
    }

    /// F-004: an empty query keeps the full available set; selection clamps to the match count.
    #[test]
    fn palette_empty_query_keeps_all_available() {
        let p = Palette::open(&normal_ctx());
        assert_eq!(p.matches.len(), p.available.len());
        assert!(!p.matches.is_empty());
    }
}
