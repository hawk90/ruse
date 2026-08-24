//! The command-line window's owned state (`:help cmdwin`): the list-overlay slice of Vim's `q:`/`q/`/`q?`.
//!
//! Vim's cmdwin is a real editable scratch buffer seeded with the command/search history — you edit any
//! line and `<CR>` runs it. A faithful port needs a hostable secondary editable buffer, which the frontend
//! does not have yet (the same multi-buffer arena the buffer/file pickers wait on). So this is the HONEST
//! reduced slice: a NAVIGABLE read-only list of the relevant history ring with Vim's trailing empty line,
//! the cursor starting on it; `<CR>` on a line EXECUTES that line's text through the very same ex/search
//! dispatch the `:`/`/` prompt uses, and `<Esc>`/`<C-c>` closes without running. In-window editing is the
//! deferred piece, pending that arena — see the change record. Behaviour verified against nvim v0.12.4
//! (`q:` lists `:` history newest-at-bottom with an empty last line, cursor on it; `<CR>` runs the line;
//! `q/`/`q?` list the search ring; `<C-c>` closes without executing).
//!
//! The `kind` glyph doubles as the dispatch selector: `:` runs an ex line, `/`/`?` run a (forward/backward)
//! search. Pure and unit-tested; the `InputEngine` owns one `Option<CmdWin>` and drives it in `feed`.

/// How many list rows the overlay shows at once. The window scrolls to keep the selection visible, so a
/// long history is still navigable within the fixed slot painted above the status line (the pum's cap).
const VIEW: usize = 10;

/// The command-line window overlay: which ring it mirrors (`kind`), the ring's entries plus Vim's trailing
/// empty line (`lines`, oldest first / most-recent last, matching the ring), and the selected line.
pub(crate) struct CmdWin {
    /// `:` (ex ring), `/` (forward search), or `?` (backward search) — also the prompt glyph and the
    /// selector deciding whether an accepted line runs as an ex command or a search.
    pub(crate) kind: char,
    /// The history entries (oldest first) followed by one empty line — Vim's extra blank last line the
    /// cursor starts on, so `<CR>` with no navigation is a harmless no-op close.
    lines: Vec<String>,
    /// The selected line, an index into `lines`. Starts on the last (empty) line, like Vim's cmdwin.
    selected: usize,
}

impl CmdWin {
    /// Open a window of `kind` over `entries` (a history ring's contents, oldest first). Appends Vim's
    /// trailing empty line and puts the cursor on it.
    pub(crate) fn open(kind: char, entries: &[String]) -> CmdWin {
        let mut lines: Vec<String> = entries.to_vec();
        lines.push(String::new()); // Vim's extra empty last line
        let selected = lines.len() - 1; // cursor starts on it
        CmdWin {
            kind,
            lines,
            selected,
        }
    }

    /// Move the selection one line toward the oldest entry (`k`/`<Up>`), bounded at the top.
    pub(crate) fn up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Move the selection one line toward the empty last line (`j`/`<Down>`), bounded at the bottom.
    pub(crate) fn down(&mut self) {
        if self.selected + 1 < self.lines.len() {
            self.selected += 1;
        }
    }

    /// The selected line's text (empty for the trailing blank line — `<CR>` there runs nothing).
    pub(crate) fn selected_line(&self) -> &str {
        &self.lines[self.selected]
    }

    /// The visible rows `(text, is_selected)` for the overlay paint slot, scrolled to keep the selection in
    /// view within [`VIEW`] rows (the list is short in practice; a full editable buffer is deferred).
    pub(crate) fn rows(&self) -> Vec<(String, bool)> {
        let len = self.lines.len();
        let start = if len <= VIEW {
            0
        } else {
            // Keep the selection as the last visible row when scrolled down, clamped to a valid window.
            self.selected.saturating_sub(VIEW - 1).min(len - VIEW)
        };
        let end = (start + VIEW).min(len);
        self.lines[start..end]
            .iter()
            .enumerate()
            .map(|(i, line)| (line.clone(), start + i == self.selected))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hist() -> Vec<String> {
        ["edit a", "set nu", "write"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn open_appends_empty_line_and_selects_it() {
        // Vim: the cmdwin has an extra empty last line and the cursor starts on it.
        let cw = CmdWin::open(':', &hist());
        assert_eq!(cw.kind, ':');
        // 3 entries + 1 empty line; selection on the empty line → running it is a no-op.
        assert_eq!(cw.selected_line(), "");
        let rows = cw.rows();
        assert_eq!(rows.len(), 4);
        assert!(rows.last().unwrap().1, "the empty last line is selected");
        // Entries appear oldest-first, matching the ring / Vim's newest-at-bottom order.
        assert_eq!(rows[0].0, "edit a");
        assert_eq!(rows[2].0, "write");
    }

    #[test]
    fn up_down_navigate_and_are_bounded() {
        let mut cw = CmdWin::open(':', &hist());
        // From the empty line, up lands on the newest entry ("write").
        cw.up();
        assert_eq!(cw.selected_line(), "write");
        cw.up();
        assert_eq!(cw.selected_line(), "set nu");
        cw.up();
        assert_eq!(cw.selected_line(), "edit a");
        cw.up(); // bounded at the top
        assert_eq!(cw.selected_line(), "edit a");
        // Down walks back toward the empty line and is bounded there.
        cw.down();
        assert_eq!(cw.selected_line(), "set nu");
        for _ in 0..5 {
            cw.down();
        }
        assert_eq!(cw.selected_line(), "", "bounded at the empty last line");
    }

    #[test]
    fn empty_history_is_just_the_blank_line() {
        let cw = CmdWin::open('/', &[]);
        assert_eq!(cw.rows().len(), 1);
        assert_eq!(cw.selected_line(), "");
    }

    #[test]
    fn rows_scroll_to_keep_selection_visible() {
        let entries: Vec<String> = (0..30).map(|i| format!("cmd{i}")).collect();
        let mut cw = CmdWin::open(':', &entries);
        // Initially the window shows the tail incl. the empty selected line.
        let rows = cw.rows();
        assert_eq!(rows.len(), VIEW);
        assert!(rows.last().unwrap().1, "selection (empty line) is visible");
        // Scroll up past the window height: the selection stays visible.
        for _ in 0..20 {
            cw.up();
        }
        let rows = cw.rows();
        assert!(
            rows.iter().any(|(_, sel)| *sel),
            "selection remains within the scrolled window"
        );
        assert_eq!(cw.selected_line(), "cmd10");
    }
}
