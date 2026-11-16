//! Per-view state setters and snapshots: cursor placement, scroll and window-local options, and the
//! mark, jump and change lists of the focused View.

use crate::editor::{CaretGravity, SubRange};
use crate::{BytePos, OptionLevel, Workspace};

impl Workspace {
    /// `:[range]mark {name}`: set the focused view's named mark at column 0 of the range's last line (the
    /// cursor's line for no range), clamped to the buffer (#1088).
    pub fn set_mark_on_line(&mut self, range: SubRange, name: char) {
        let vid = self.windows[self.focus].view;
        let did = self.focused_buffer();
        let b = self.docs[Self::doc_slot(did)]
            .as_ref()
            .expect("focused doc live")
            .bytes();
        let lines = crate::pos::count_newlines(b) + usize::from(b.last() != Some(&b'\n'));
        let view = self.views[vid.0].as_mut().expect("focused view live");
        let line = match range {
            SubRange::CurrentLine => crate::pos::raw::line_of(b, view.cursor().0),
            SubRange::WholeFile => lines.saturating_sub(1),
            SubRange::Lines(_, last) => last.clamp(1, lines.max(1)) - 1,
        };
        view.set_named_mark(name, crate::pos::raw::nth_line_start(b, line));
    }

    /// Place the FOCUSED view's cursor at byte offset `pos` (the frontend uses this to scroll a
    /// `:s///c` match into view during the confirm loop). Clamped to the buffer.
    pub fn place_focused_cursor(&mut self, pos: BytePos) {
        let did = self.focused_buffer();
        let len = self.docs[Self::doc_slot(did)]
            .as_ref()
            .expect("focused doc live")
            .bytes()
            .len();
        if let Some(v) = self.window_view_mut(self.focus) {
            v.set_cursor(pos.min(BytePos(len)));
        }
    }

    /// Record `from` on the FOCUSED view's jumplist (Vim's `setpcmark`), for a jump the frontend runs: a
    /// `:s` that found a match (#940).
    pub fn push_focused_jump(&mut self, from: BytePos) {
        if let Some(v) = self.window_view_mut(self.focus) {
            v.push_jump(from.0);
        }
    }

    /// Select the caret gravity for every live View (D-050 / RFC-0015): the frontend calls this once at
    /// startup with `BetweenChar` when the Emacs profile is active, so its edits rest on Emacs point rather
    /// than being Vim-clamped. Splits clone the focused View, so a later `:split` inherits the gravity.
    pub fn set_caret_gravity(&mut self, gravity: CaretGravity) {
        self.defaults.set_caret(gravity);
        for v in self.views.iter_mut().flatten() {
            v.set_caret_gravity(gravity);
        }
    }

    /// Set the `i`th Window's scroll position (the frontend viewport pass, after computing it from the
    /// pane's cursor and its sub-rectangle height).
    pub fn set_top(&mut self, i: usize, top: usize) {
        if let Some(v) = self.window_view_mut(i) {
            v.set_top(top);
        }
    }

    /// Set the `i`th Window's text-area width, which `'wrapmargin'` and a `gq` without `'textwidth'`
    /// measure against (#618).
    pub fn set_text_cols(&mut self, i: usize, cols: usize) {
        if let Some(v) = self.window_view_mut(i) {
            v.set_text_cols(cols);
        }
    }

    /// Set the `i`th Window's first displayed column when `'wrap'` is off (the frontend's horizontal
    /// scroll, #615).
    pub fn set_leftcol(&mut self, i: usize, col: usize) {
        if let Some(v) = self.window_view_mut(i) {
            v.set_leftcol(col);
        }
    }

    /// Set the `i`th Window's skipped virtual columns of its top line (`'smoothscroll'`, #615).
    pub fn set_skipcol(&mut self, i: usize, col: usize) {
        if let Some(v) = self.window_view_mut(i) {
            v.set_skipcol(col);
        }
    }

    /// The global copy of the window-local display options (#615, #616).
    #[must_use]
    pub fn window_defaults(&self) -> &crate::WindowOptions {
        &self.window_defaults
    }

    /// Change the window-local display options at `level`: the focused window's own copy (`:setlocal`),
    /// the global copy new windows start from (`:setglobal`), or both (`:set`).
    pub fn update_window_options(
        &mut self,
        level: OptionLevel,
        mut change: impl FnMut(&mut crate::WindowOptions),
    ) {
        if level != OptionLevel::Local {
            change(&mut self.window_defaults);
        }
        if level != OptionLevel::Global {
            if let Some(v) = self.window_view_mut(self.focus) {
                let mut opts = v.window_options().clone();
                change(&mut opts);
                v.set_window_options(opts);
            }
        }
    }

    /// Set (or clear) the FOCUSED Window's own `'scrolloff'` (`:setlocal so`, #1151).
    pub fn set_focused_scrolloff(&mut self, so: Option<i64>) {
        if let Some(v) = self.window_view_mut(self.focus) {
            v.set_scrolloff_local(so);
        }
    }

    /// Set the `i`th Window's 'scroll' amount (`{count}CTRL-D`), valid while its height stays `height`.
    pub fn set_scroll(&mut self, i: usize, lines: usize, height: usize) {
        if let Some(v) = self.window_view_mut(i) {
            v.set_scroll(lines, height);
        }
    }

    /// Place the FOCUSED view's cursor at byte offset `pos` the way [`EditorState::set_cursor`] does:
    /// clamped, snapped to a char boundary, and seeding the sticky column, so a following vertical move
    /// aims at `pos`'s column (the swap-trick). For homing a test fixture's start point;
    /// [`Workspace::place_focused_cursor`] leaves the sticky column alone.
    pub fn set_focused_cursor(&mut self, pos: BytePos) {
        self.with_focused(|st| st.set_cursor(pos));
    }

    /// The FOCUSED view's set marks (`a`-`z`, `.`, `^`) as `(name, byte offset)` for `:marks` (swap-trick).
    pub fn marks_snapshot(&mut self) -> Vec<(char, BytePos)> {
        self.with_focused(|st| st.marks_snapshot())
    }

    /// The FOCUSED view's jumplist positions for `:jumps` (swap-trick).
    pub fn jumps_snapshot(&mut self) -> Vec<BytePos> {
        self.with_focused(|st| st.jumps_snapshot())
    }

    /// The FOCUSED view's change-list positions for `:changes` (swap-trick).
    pub fn changes_snapshot(&mut self) -> Vec<BytePos> {
        self.with_focused(|st| st.changes_snapshot())
    }

    /// `:keepjumps` (#609): while `on`, no view records a jumplist or change-list entry (so `''`, `` `. ``,
    /// `CTRL-O` and `g;` are as they were before the command).
    pub fn set_keep_jumps(&mut self, on: bool) {
        for view in self.views.iter_mut().flatten() {
            view.keep_jumps = on;
        }
    }

    /// Whether `:keepjumps` is in force (see [`Workspace::set_keep_jumps`]).
    #[must_use]
    pub fn keep_jumps(&self) -> bool {
        self.views.iter().flatten().any(|v| v.keep_jumps)
    }
}
