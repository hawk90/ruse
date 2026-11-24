//! Read-only text queries at the focused cursor: the word under it, completion candidates, and the
//! line spans a motion or command covers.

use crate::Workspace;

impl Workspace {
    /// The 0-based inclusive line range a `=`/`Reindent` motion covers on the focused buffer (its op-span's
    /// first/last line), or `None` if the span is empty. The frontend uses this to compute tree-aware indent
    /// levels for the same lines the core's bracket-depth `=` would reindent.
    #[must_use]
    pub fn reindent_range(&self, motion: crate::Motion, count: u32) -> Option<(usize, usize)> {
        let pane = self.focused();
        let b = pane.doc.bytes();
        let (s, e, _) = crate::editor::op_span(b, pane.view.cursor().0, motion, count);
        if s >= e {
            return None;
        }
        Some((
            crate::pos::raw::line_of(b, s),
            crate::pos::raw::line_of(b, e - 1),
        ))
    }

    /// The 0-based inclusive line range of the focused buffer that the operator span of `cmd` covers (a
    /// search, `n`/`N`, `*`/`#`, a `gn` match or a mark), or `None` when it has no target. See
    /// [`EditorState::span_lines`]; the frontend numbers the `!{motion}` filter's `:{range}!` line from it.
    pub fn span_lines(&mut self, cmd: &crate::Command) -> Option<(usize, usize)> {
        self.with_focused(|st| st.span_lines(cmd))
    }

    /// The search pattern `*`/`#` (`whole_word`, `\<word\>`) or `g*`/`g#` (the bare word) builds from the
    /// keyword under the focused cursor — the same pattern the planner searches for
    /// [`Command::SearchWordUnder`](crate::Command::SearchWordUnder). `None` when the line has no keyword
    /// at or after the cursor (E348).
    #[must_use]
    pub fn word_search_pattern(&self, whole_word: bool) -> Option<String> {
        let pane = self.focused();
        crate::editor::word_search(pane.doc.bytes(), pane.view.cursor().0, whole_word)
            .map(|(_, p)| p)
    }

    /// The search Visual `*`/`#` ([`Command::SearchSelection`](crate::Command::SearchSelection)) runs over
    /// the focused selection: its `\V` pattern (empty when nothing is selected, E348) and whether a backward
    /// search counts the selection once more. `None` outside Visual. See [`crate::View::selection_search`].
    #[must_use]
    pub fn selection_search(&self) -> Option<(String, bool)> {
        let pane = self.focused();
        pane.view.selection_search(pane.doc.bytes())
    }

    /// The keyword under (or forward-on-line from) the focused cursor — the pattern source for Vim `*`/`#`.
    /// `None` when the current line has no keyword at/after the cursor. A byte that is not UTF-8 is
    /// spelled as the regex engine's escape char ([`crate::pattern::spell`]), so the text is a pattern.
    #[must_use]
    pub fn word_under_cursor(&self) -> Option<String> {
        let pane = self.focused();
        let b = pane.doc.bytes();
        let (s, e) = crate::motion::word_under_cursor(b, pane.view.cursor().0)?;
        Some(crate::pattern::spell(&b[s..e]).into_owned())
    }

    /// The `<cword>` under the focused caret — the keyword (or, failing that, non-blank run) Vim's
    /// command-line `c_CTRL-R_CTRL-W` splices at the cmdline caret. `None` when the rest of the line is
    /// blank. See [`crate::motion::cword_under_cursor`] for the exact (nvim-verified) resolution.
    #[must_use]
    pub fn cword_under_cursor(&self) -> Option<String> {
        self.cword_impl(false)
    }

    /// The `<cWORD>` under the focused caret — the whitespace-delimited WORD Vim's command-line
    /// `c_CTRL-R_CTRL-A` splices at the cmdline caret. `None` when the rest of the line is blank.
    #[must_use]
    pub fn cbig_word_under_cursor(&self) -> Option<String> {
        self.cword_impl(true)
    }

    fn cword_impl(&self, big: bool) -> Option<String> {
        let pane = self.focused();
        let b = pane.doc.bytes();
        let (s, e) = crate::motion::cword_under_cursor(b, pane.view.cursor().0, big)?;
        std::str::from_utf8(&b[s..e]).ok().map(str::to_string)
    }

    /// The character on the line directly ABOVE (`above == true`) or BELOW the focused caret, at the
    /// caret's current column — the buffer value the frontend needs to resolve `i_CTRL-Y` / `i_CTRL-E`
    /// (the engine has no buffer). Returns `None` when there is no such line, or the adjacent line has no
    /// character at that column (a Vim no-op / bell — verified against nvim v0.12.4).
    ///
    /// The column is the caret's VIRTUAL column, as nvim's `ins_copychar` measures it: the char whose cells
    /// cover it on the adjacent line (a tab, a wide char, a byte that is not UTF-8's `<ff>`, #1252). A byte
    /// that is not UTF-8 there is copied as the Latin-1 char of its value, as nvim inserts it (`ÿ` for
    /// 0xFF). A cluster gives its first char.
    #[must_use]
    pub fn adjacent_line_char(&self, above: bool) -> Option<char> {
        let pane = self.focused();
        let b = pane.doc.bytes();
        let cur = pane.view.cursor().0.min(b.len());
        let tabw = pane.view.tabs();
        let vcol = crate::editor::vcols_at(b, cur, tabw).0;
        // The adjacent row. `checked_sub` guards "no line above" on the first line; a row past the last
        // line resolves to an empty slice below, which yields no char (so "no line below" also → None).
        let row = crate::pos::raw::line_of(b, cur);
        let target = if above { row.checked_sub(1)? } else { row + 1 };
        let ts = crate::pos::raw::nth_line_start(b, target);
        let at = crate::editor::want_col(b, ts, vcol, tabw);
        if at >= crate::pos::raw::line_end(b, ts) {
            return None;
        }
        match crate::motion::char_len_at(b, at) {
            Some(w) => std::str::from_utf8(&b[at..at + w]).ok()?.chars().next(),
            None => Some(char::from(b[at])),
        }
    }

    /// Insert-mode keyword completion source for `i_CTRL-N` / `i_CTRL-P` (F-003, current-buffer only).
    /// Returns `(base, candidates)`: `base` is the keyword text immediately before the focused caret (the
    /// text being completed, empty if none), and `candidates` are the buffer keywords that START WITH it,
    /// in nvim's scan order — forward from the caret then wrapping, the word under construction excluded,
    /// duplicates removed (see [`crate::motion::keyword_completions`]). The engine has no buffer, so the
    /// frontend calls this and hands both to the input engine, which cycles the selection. An EMPTY
    /// candidate list means "no match" (a Vim no-op / bell).
    #[must_use]
    pub fn keyword_completion(&self) -> (String, Vec<String>) {
        let pane = self.focused();
        let b = pane.doc.bytes();
        let cur = pane.view.cursor().0;
        let (_, base) = crate::motion::keyword_before_cursor(b, cur);
        let cands = crate::motion::keyword_completions(b, cur, base);
        // The base travels spelled like the candidates (a byte that is not UTF-8 as its escape char): its
        // char count is what the first step deletes (#1252).
        (crate::pattern::spell(base).into_owned(), cands)
    }

    /// Insert-mode WHOLE-LINE completion source for `i_CTRL-X CTRL-L` (F-003, current-buffer only).
    /// Returns `(base, candidates)` for the focused view: `base` is the caret line's post-indent text up to
    /// the caret (the text being completed), and `candidates` are the OTHER buffer lines' post-indent text
    /// that START WITH `base`, in Vim's backward-from-caret-with-wrap scan order, deduped by post-indent
    /// content (see [`crate::motion::line_completions`]). Leading indent is ignored on both sides. The
    /// engine has no buffer, so the frontend calls this and hands both to the input engine, which cycles the
    /// selection exactly as for [`keyword_completion`](Self::keyword_completion). An EMPTY candidate list
    /// means "no match" (a Vim no-op / bell).
    #[must_use]
    pub fn line_completion(&self) -> (String, Vec<String>) {
        let pane = self.focused();
        let b = pane.doc.bytes();
        let cur = pane.view.cursor().0;
        crate::motion::line_completions(b, cur)
    }
}
