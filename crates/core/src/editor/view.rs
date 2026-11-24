//! The `View` methods: offset snapping and tracked positions, invariants, marks, the change list, jumps,
//! and the per-window option accessors. Moved out of `editor/mod.rs` (#1379); a child module, so it reads
//! the private `View` fields as before.

use super::{
    block_geom, cindent, indent, mark_slot, planner, selection_range, ViewOptions, Want, MAXCOL,
    MAX_CHANGES, TRACKED_FIXED,
};
use crate::motion::{line_start, snap};
use crate::{
    BytePos, CaretGravity, CinKey, Command, DocumentId, EditorOption, Mode, RegisterStore,
    SelectKind, Tabs, Tracked, View,
};

impl View {
    /// Clamp and snap every stored offset (change list, marks, `gi`, `gv`, `[`/`]`, jumps) into `bytes`, so an
    /// edit that shrank the buffer or split a character under one can never make a later jump slice out of
    /// bounds or inside a UTF-8 sequence.
    pub(crate) fn snap_offsets(&mut self, bytes: &[u8]) {
        let len = bytes.len();
        for c in self.changes.iter_mut() {
            *c = snap(bytes, (*c).min(len));
        }
        // Named marks (`m{a-z}`) snap into range too, so an edit that shrank the buffer under a mark can never
        // make a later `` `{a-z} `` jump out of bounds.
        for m in self.named_marks.iter_mut().flatten() {
            *m = snap(bytes, (*m).min(len));
        }
        // The last-insert position (`gi`) snaps the same way.
        if let Some(li) = self.last_insert {
            self.last_insert = Some(snap(bytes, li.min(len)));
        }
        // The last-visual store (behind `gv` and the `<`/`>` marks) snaps both ends into range so an edit that
        // shrank the buffer under a remembered selection can never make a later `gv`/`` `< ``/`` `> `` slice out
        // of bounds. (Vim keeps the raw columns; snapping only clamps out-of-buffer offsets, never in-range ones.)
        if let Some((a, e, kind)) = self.last_visual {
            self.last_visual = Some((snap(bytes, a.min(len)), snap(bytes, e.min(len)), kind));
        }
        // The `[`/`]` change marks, an in-flight Insert-session start, the Visual anchor and the Emacs mark
        // snap into range too.
        for p in [
            &mut self.anchor,
            &mut self.mark,
            &mut self.change_start,
            &mut self.change_end,
            &mut self.insert_start,
            &mut self.insstart,
        ]
        .into_iter()
        .flatten()
        {
            *p = snap(bytes, (*p).min(len));
        }
        // Jumplist entries snap into range too (a buffer-resizing edit under a jump keeps it valid).
        for j in self.jumps.iter_mut() {
            *j = snap(bytes, (*j).min(len));
        }
    }

    /// Every long-lived position this View keeps, for its Document to carry through a change as nvim's
    /// `mark_adjust` does (#686), in the order [`View::untrack`] reads back: `a`–`z` (dropped with their
    /// line, restored by undo), the last Visual area (restored by undo), `A`–`Z`, `` `^ `` (dropped), `[`
    /// and `]`, `0`–`9` (kept, as file marks are), then the change list and the jumplist. The cursor is the command's to place, so it is not
    /// here; the Emacs mark stays a snapped offset (a byte anchor moved it off Emacs's in-place casing).
    pub(crate) fn tracked(&self) -> Vec<Tracked> {
        use crate::anchor::{LineGravity::Drop, LineGravity::Keep};
        let mut t = Vec::with_capacity(TRACKED_FIXED + self.changes.len() + self.jumps.len());
        t.extend(
            self.named_marks[..26]
                .iter()
                .map(|&m| Tracked::line(m.map(BytePos), Drop).restored()),
        );
        let (va, ve) = self
            .last_visual
            .map_or((None, None), |(a, e, _)| (Some(a), Some(e)));
        t.push(Tracked::line(va.map(BytePos), Keep).restored());
        t.push(Tracked::line(ve.map(BytePos), Keep).restored());
        t.extend(
            self.named_marks[26..52]
                .iter()
                .map(|&m| Tracked::line(m.map(BytePos), Keep)),
        );
        t.push(Tracked::line(self.last_insert.map(BytePos), Drop));
        t.push(Tracked::line(self.change_start.map(BytePos), Keep));
        t.push(Tracked::line(self.change_end.map(BytePos), Keep));
        t.extend(
            self.named_marks[52..]
                .iter()
                .map(|&m| Tracked::line(m.map(BytePos), Keep)),
        );
        debug_assert_eq!(t.len(), TRACKED_FIXED);
        t.extend(
            self.changes
                .iter()
                .map(|&p| Tracked::line(Some(BytePos(p)), Keep)),
        );
        t.extend(
            self.jumps
                .iter()
                .map(|&p| Tracked::line(Some(BytePos(p)), Keep)),
        );
        t
    }

    /// Take back the positions [`View::tracked`] gave out, as the Document left them.
    pub(crate) fn untrack(&mut self, t: &[Tracked]) {
        let pos = |i: usize| t.get(i).and_then(|t| t.pos).map(|p| p.0);
        for (i, m) in self.named_marks[..26].iter_mut().enumerate() {
            *m = pos(i);
        }
        if let (Some((_, _, kind)), Some(a), Some(e)) = (self.last_visual, pos(26), pos(27)) {
            self.last_visual = Some((a, e, kind));
        }
        for (i, m) in self.named_marks[26..52].iter_mut().enumerate() {
            *m = pos(26 + i + 2);
        }
        self.last_insert = pos(54);
        self.change_start = pos(55);
        self.change_end = pos(56);
        for (i, m) in self.named_marks[52..62].iter_mut().enumerate() {
            *m = pos(52 + i + 5);
        }
        let n = self.changes.len();
        for (i, c) in self.changes.iter_mut().enumerate() {
            *c = pos(TRACKED_FIXED + i).unwrap_or(0);
        }
        for (i, j) in self.jumps.iter_mut().enumerate() {
            *j = pos(TRACKED_FIXED + n + i).unwrap_or(0);
        }
    }

    /// The invariant walker for one view over `bytes` (#906): the cursor and every stored position (marks,
    /// change list, jumplist, `[`/`]`, `gi`, `gv`, the Visual anchor, the Emacs mark) lie inside the text
    /// and on a char boundary of valid UTF-8, and the change and jump cursors stay within their lists.
    /// Panics on the first broken rule; for property and fuzz tests.
    pub fn assert_invariants(&self, bytes: &[u8]) {
        let len = bytes.len();
        let utf8 = std::str::from_utf8(bytes).ok();
        let check = |what: &str, p: usize| {
            assert!(p <= len, "{what} at {p}, past the end {len}");
            if let Some(s) = utf8 {
                assert!(s.is_char_boundary(p), "{what} at {p} splits a character");
            }
        };
        check("the cursor", self.cursor);
        for (i, t) in self.tracked().iter().enumerate() {
            if let Some(p) = t.pos {
                check(&format!("tracked position {i}"), p.0);
            }
        }
        for (what, p) in [
            ("the Visual anchor", self.anchor),
            ("the Emacs mark", self.mark),
            ("the Insert start", self.insert_start),
            ("Insstart", self.insstart),
        ] {
            if let Some(p) = p {
                check(what, p);
            }
        }
        assert!(self.changes.len() <= MAX_CHANGES, "change list overflow");
        assert!(self.jumps.len() <= MAX_CHANGES, "jumplist overflow");
        assert!(
            self.change_idx <= self.changes.len(),
            "change cursor past the list"
        );
        assert!(
            self.jump_idx <= self.jumps.len(),
            "jump cursor past the list"
        );
    }

    /// A fresh view over `doc`: cursor at the start, Normal mode, empty registers/sessions. Config
    /// fields hold the schema defaults (spec/config-schema.yaml: editor.tab_width=4,
    /// editor.indent_style=space); runtime config wiring is deferred (as with editor.scrolloff).
    pub(crate) fn fresh(doc: DocumentId) -> View {
        View {
            doc,
            top: 0,
            scroll: None,
            scrolloff: None,
            leftcol: 0,
            skipcol: 0,
            window: crate::WindowOptions::default(),
            cursor: 0,
            mode: Mode::Normal,
            last_was_edit: false,
            last_was_kill: false,
            registers: RegisterStore::new(),
            pending_register: None,
            anchor: None,
            mark: None,
            last_visual: None,
            visual_mode_eval: None,
            replace_stack: Vec::new(),
            block_insert: None,
            last_block_geom: None,
            last_visual_op: None,
            auto_indent_pending: false,
            si_state: indent::SiState::default(),
            typed_space: false,
            old_indent: 0,
            end_comment: None,
            indent: ViewOptions::DEFAULT.indent,
            text_width: ViewOptions::DEFAULT.text_width,
            format: ViewOptions::DEFAULT.format,
            text_cols: 80,
            curswant: Want::col(0),
            keep_jumps: false,
            search_case: ViewOptions::DEFAULT.search_case,
            caret: ViewOptions::DEFAULT.caret,
            changes: Vec::new(),
            new_change: false,
            change_idx: 0,
            named_marks: [None; 62],
            visual_one_end: None,
            last_insert: None,
            change_start: None,
            change_end: None,
            insert_start: None,
            insert_col: 0,
            insstart: None,
            insstart_line: None,
            insert_jumped: false,
            jumps: Vec::new(),
            jump_idx: 0,
        }
    }

    /// The last-insert position (Vim `` `^ ``), or `None` before any Insert session has ended.
    pub(super) fn last_insert(&self) -> Option<usize> {
        self.last_insert
    }

    /// The fixed end of the Visual area being made (nvim's `getpos('v')`; the cursor is the other end), or
    /// `None` outside Visual and Select mode. The parity comparator reads it (#900).
    #[must_use]
    pub fn visual_anchor(&self) -> Option<BytePos> {
        self.anchor.map(BytePos)
    }

    /// The byte offset of mark `c`, or `None` if unset or not a mark this resolves: a letter (`a`–`z`,
    /// `A`–`Z`), `^` (where Insert stopped) or `<`/`>` (the last Visual area's start / end; a linewise one
    /// from its first line's start to its last line's end). The marks a jump or an operator reads by name
    /// (`` `^ ``, `` d`> ``, #1088).
    #[must_use]
    pub fn named_mark(&self, c: char) -> Option<BytePos> {
        match c {
            '^' => self.last_insert,
            '<' | '>' => {
                let (s, e, _) = self.last_visual_bounds()?;
                return Some(if c == '<' { s } else { e });
            }
            _ => self.named_marks[mark_slot(c)?],
        }
        .map(BytePos)
    }

    /// Set named mark `c` (`a`–`z`, `A`–`Z`) to `pos`. Any other `c` is ignored (the input layer only sends
    /// letters, so this is a defensive guard).
    pub(crate) fn set_named_mark(&mut self, c: char, pos: usize) {
        if let Some(i) = mark_slot(c) {
            self.named_marks[i] = Some(pos);
        }
    }

    /// Set mark `c` at `pos` the way nvim's `setmark_pos` does for `:mark` (#1241): a letter or digit is
    /// that named or numbered mark, `'`/`` ` `` the context mark (a jumplist entry), `[`/`]` a change mark
    /// and `<`/`>` one end of the last Visual area (with none, the only end). `false` for any other `c`,
    /// which sets nothing (`E191`).
    pub(crate) fn set_mark(&mut self, c: char, pos: usize) -> bool {
        match c {
            '\'' | '`' => self.push_jump(pos),
            '[' => self.change_start = Some(pos),
            ']' => self.change_end = Some(pos),
            '<' | '>' => {
                let start = c == '<';
                self.last_visual = Some(match (self.last_visual, self.visual_one_end) {
                    // The other end is still unset: this one is the only end again.
                    (Some((_, _, kind)), Some(only)) if only == c => (pos, pos, kind),
                    // The end that was missing: the area is whole now.
                    (Some((a, _, kind)), Some(_)) => {
                        self.visual_one_end = None;
                        if start {
                            (pos, a, kind)
                        } else {
                            (a, pos, kind)
                        }
                    }
                    (Some((a, e, kind)), None) => {
                        if start {
                            (pos, e, kind)
                        } else {
                            (a, pos, kind)
                        }
                    }
                    (None, _) => {
                        self.visual_one_end = Some(c);
                        (pos, pos, SelectKind::Charwise)
                    }
                });
            }
            _ if mark_slot(c).is_some() => self.set_named_mark(c, pos),
            _ => return false,
        }
        true
    }

    /// The position of the last change (Vim `` `. ``), or `None` before the first edit.
    #[must_use]
    pub fn last_change(&self) -> Option<BytePos> {
        self.changes.last().copied().map(BytePos)
    }

    /// Vim's `[` mark — the first char of the last changed/yanked text, or `None` before the first change.
    pub fn change_mark_start(&self) -> Option<BytePos> {
        self.change_start.map(BytePos)
    }

    /// Vim's `]` mark — the end of the last changed/yanked text (see [`View::change_end`]), or `None`.
    pub fn change_mark_end(&self) -> Option<BytePos> {
        self.change_end.map(BytePos)
    }

    /// The last visual selection's bounds `(start, end, kind)` — the byte-min and byte-max of the remembered
    /// selection's two ends, in its kind. This is the store behind Vim's `<`/`>` marks (`` `< ``/`` `> ``/
    /// `'<`/`'>`) and `gv`, all reading the one depth-1 `last_visual` (set on every Visual/Select exit). `None`
    /// before any visual selection has ended. The planner turns `(start, end, kind)` into the per-mark target.
    pub fn last_visual_bounds(&self) -> Option<(BytePos, BytePos, SelectKind)> {
        let (a, e, kind) = self.last_visual?;
        Some((BytePos(a.min(e)), BytePos(a.max(e)), kind))
    }

    /// The kind Vim's `visualmode()` reports for the last Visual area (see [`View::visual_mode_eval`]), or
    /// `None` before any Visual area has been left.
    pub fn visual_mode(&self) -> Option<SelectKind> {
        self.visual_mode_eval
    }

    /// The context mark (Vim `` ` ``/`'`): the position before the latest jump — the newest jumplist entry.
    /// `None` before any jump. Read by `` `` ``/`''`; the jump they perform then records a fresh entry, so
    /// repeating toggles between the two positions.
    pub fn context_mark(&self) -> Option<BytePos> {
        self.jumps.last().copied().map(BytePos)
    }

    /// Record a change at `pos` in the change list, as Vim's `changed_common` does (#722): a change that
    /// began a new undo step (`new_step`) takes a new entry unless it is on the newest entry's line within
    /// `'textwidth'` (79 when unset) columns of it; every change then moves the newest entry to `pos`. So
    /// typing a word, or `xxx`, leaves one entry. The list is bounded to [`MAX_CHANGES`] (oldest dropped).
    /// Resets the `g;`/`g,` cursor past the newest entry.
    pub(super) fn push_change(&mut self, b: &[u8], pos: usize, new_step: bool) {
        if self.keep_jumps {
            return;
        }
        self.new_change |= new_step;
        if self.new_change || self.changes.is_empty() {
            let line_col = |p: usize| {
                let p = p.min(b.len());
                let ls = line_start(b, p);
                (crate::pos::raw::line_of(b, p), p - ls)
            };
            let add = self.changes.last().is_none_or(|&last| {
                let ((line, col), (l, c)) = (line_col(pos), line_col(last));
                let cols = if self.text_width == 0 {
                    79
                } else {
                    self.text_width
                };
                l != line || c + cols < col || col + cols < c
            });
            if add {
                self.new_change = false;
                self.changes.push(pos);
                if self.changes.len() > MAX_CHANGES {
                    self.changes.remove(0);
                }
            }
        }
        if let Some(last) = self.changes.last_mut() {
            *last = pos;
        }
        self.change_idx = self.changes.len();
    }

    /// Step the change-list cursor (`g;` = `older`, `g,` = newer) and return the position to jump to, or
    /// `None` when there is nowhere to go (empty list, or already at the oldest/newest end).
    pub(super) fn nav_change(&mut self, older: bool) -> Option<usize> {
        if self.changes.is_empty() {
            return None;
        }
        if older {
            if self.change_idx == 0 {
                return None; // already at the oldest change
            }
            self.change_idx -= 1;
        } else {
            if self.change_idx + 1 >= self.changes.len() {
                return None; // already at (or past) the newest change
            }
            self.change_idx += 1;
        }
        self.changes.get(self.change_idx).copied()
    }

    /// Record `from` as a jumplist entry — the position a jump command LEFT (Vim's `setpcmark`). Adjacent
    /// identical positions coalesce; bounded to [`MAX_CHANGES`]; resets the `CTRL-O`/`CTRL-I` cursor past
    /// the newest. Entries on one line are folded when the list is next walked ([`View::clean_jumps`]).
    pub(crate) fn push_jump(&mut self, from: usize) {
        if self.keep_jumps {
            return;
        }
        if self.jumps.last() != Some(&from) {
            self.jumps.push(from);
            if self.jumps.len() > MAX_CHANGES {
                self.jumps.remove(0);
            }
        }
        self.jump_idx = self.jumps.len();
    }

    /// Vim's `cleanup_jumplist` (#725): drop every entry that a LATER entry shares a line with, so a line
    /// appears once, at its newest place; then, with the cursor past the newest entry, drop that entry when
    /// it is on the cursor's line `now` (a phantom jump, nvim #9805). The walk cursor follows its entry.
    fn clean_jumps(&mut self, b: &[u8], now: usize) {
        let line = |p: usize| crate::pos::raw::line_of(b, p.min(b.len()));
        let lines: Vec<usize> = self.jumps.iter().map(|&p| line(p)).collect();
        let len = self.jumps.len();
        let mut kept = Vec::with_capacity(len);
        for (from, &pos) in self.jumps.iter().enumerate() {
            if self.jump_idx == from {
                self.jump_idx = kept.len();
            }
            if !lines[from + 1..].contains(&lines[from]) {
                kept.push(pos);
            }
        }
        if self.jump_idx == len {
            self.jump_idx = kept.len();
        }
        self.jumps = kept;
        if self.jump_idx == self.jumps.len()
            && self.jumps.last().is_some_and(|&p| line(p) == line(now))
        {
            self.jumps.pop();
            self.jump_idx -= 1;
        }
    }

    /// Step the jumplist (`CTRL-O` = `older`, `CTRL-I` = newer) from the current position `now`, returning
    /// the position to jump to or `None` at an end — nvim's `get_jumplist`. The list is cleaned first
    /// ([`View::clean_jumps`]). The FIRST `CTRL-O` saves `now` onto the list so a later `CTRL-I` returns to
    /// it; subsequent steps just walk the cursor.
    pub(super) fn nav_jump(&mut self, older: bool, now: usize, b: &[u8]) -> Option<usize> {
        self.clean_jumps(b, now);
        let len = self.jumps.len();
        if older {
            if self.jump_idx == 0 || len == 0 {
                return None; // already at the oldest jump
            }
            if self.jump_idx == len {
                // First move back: remember where we are so `CTRL-I` can return, then step onto the list.
                self.jumps.push(now);
                if self.jumps.len() > MAX_CHANGES {
                    self.jumps.remove(0);
                }
                self.jump_idx = self.jumps.len() - 1;
                if self.jump_idx == 0 {
                    return None;
                }
            }
            self.jump_idx -= 1;
        } else {
            if self.jump_idx + 1 >= len {
                return None; // already at (or past) the newest jump
            }
            self.jump_idx += 1;
        }
        self.jumps.get(self.jump_idx).copied()
    }

    /// The regex compile options for a search in this view (magic default; case per config).
    #[must_use]
    pub fn search_options(&self) -> crate::pattern::Options {
        crate::pattern::Options {
            magic: crate::pattern::Magic::Magic,
            ignore_case: self.search_case.ignore,
            smart_case: self.search_case.smart,
        }
    }

    /// Whether a search wraps around the buffer's end (`'wrapscan'`, #1558).
    #[must_use]
    pub fn wrapscan(&self) -> bool {
        self.search_case.wrap
    }

    /// The buffer this View shows.
    #[must_use]
    pub fn doc(&self) -> DocumentId {
        self.doc
    }

    /// The cursor's byte offset (on a char boundary) — view-local.
    #[must_use]
    pub fn cursor(&self) -> BytePos {
        BytePos(self.cursor)
    }

    /// The Emacs mark's byte offset, or `None` when no mark is set — view-local (see
    /// [`EditorState::mark`]).
    #[must_use]
    pub fn mark(&self) -> Option<BytePos> {
        self.mark.map(BytePos)
    }

    /// The one-shot register a `"x` selected for the next command, or `None` for the unnamed one.
    #[must_use]
    pub fn pending_register(&self) -> Option<char> {
        self.pending_register
    }

    /// Place the cursor at byte offset `pos` (the frontend uses this to follow a `:s///c` match; the
    /// caller passes an on-boundary offset). View-local; does not touch the document.
    pub fn set_cursor(&mut self, pos: BytePos) {
        self.cursor = pos.0;
    }

    /// Select this View's caret gravity (D-050): the Emacs profile constructs `BetweenChar` views so its
    /// edits are not Vim-clamped; Vim/Neovim keep the `OnChar` default. The Workspace profile-init seam.
    pub fn set_caret_gravity(&mut self, gravity: CaretGravity) {
        self.caret = gravity;
    }

    /// This View's caret gravity (D-050).
    #[must_use]
    pub fn caret_gravity(&self) -> CaretGravity {
        self.caret
    }

    /// This View's option values ([`ViewOptions`]).
    pub(crate) fn options(&self) -> ViewOptions {
        ViewOptions {
            indent: self.indent,
            text_width: self.text_width,
            format: self.format.clone(),
            search_case: self.search_case,
            caret: self.caret,
        }
    }

    /// Replace this View's option values, as a new View takes the Workspace's global ones (#754).
    pub(crate) fn set_options(&mut self, o: ViewOptions) {
        self.indent = o.indent;
        self.text_width = o.text_width;
        self.format = o.format;
        self.search_case = o.search_case;
        self.caret = o.caret;
    }

    /// The values of every option the core reads while editing (`ic`, `scs`, `sw`, `ts`, `et`, `tw`, and
    /// the caret gravity), as the `:set`s that would install them. A trace writes them at its start, so replay does not depend on
    /// the replaying machine's config (RFC-0020 R4). `sw` is the raw value (`0` follows `ts`).
    #[must_use]
    pub fn editing_options(&self) -> Vec<EditorOption> {
        self.options().editing_options()
    }

    /// Change this View's option values through `f`.
    pub(crate) fn update_options(&mut self, f: impl FnOnce(&mut ViewOptions)) {
        let mut o = self.options();
        f(&mut o);
        self.set_options(o);
    }

    /// This View's mode — view-local (two Views of one buffer can be in different modes).
    #[must_use]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// This View's `'tabstop'` in columns (`editor.tab_width`, `:set tabstop`): how wide a `\t` renders. A
    /// read-only config accessor: the frontend reads it to paint tabs and place the caret, and with
    /// [`View::shift_width`] to compute `foldmethod=indent` levels (folds stay a frontend concern — core
    /// exposes the numbers, never the folds; INV-DOC-VIEW).
    #[must_use]
    pub fn tab_width(&self) -> usize {
        self.indent.tab_width
    }

    /// This View's tab stops: `'tabstop'` and `'vartabstop'` (#1163). Every width that meets a `<Tab>`
    /// (rendering, cursor columns, indents) is measured with these.
    #[must_use]
    pub fn tabs(&self) -> Tabs {
        self.indent.tabs()
    }

    /// This View's effective `'shiftwidth'` in columns (`editor.shift_width`, `:set shiftwidth`): one indent
    /// level. A `shiftwidth` of `0` resolves to the tabstop (Vim's `sw=0`), so this is always `>= 1`.
    #[must_use]
    pub fn shift_width(&self) -> usize {
        self.indent.shift_width()
    }

    /// Whether `'lisp'` is on: the core indents by the Lisp rules, so the frontend's indenters stand
    /// aside (#1163).
    #[must_use]
    pub fn lisp(&self) -> bool {
        self.format.lisp
    }

    /// Whether `'cindent'` is on (#1163).
    #[must_use]
    pub fn cindent(&self) -> bool {
        self.format.cindent
    }

    /// The C indent of line `row` of `bytes` under this View's options (nvim's `get_c_indent`, #1163),
    /// the cursor at byte `col` of the line (in Insert mode a `)` there is cut off first). `None` keeps
    /// the line as it is (inside a C++ raw string).
    #[must_use]
    pub fn c_indent(&self, bytes: &[u8], row: usize, col: usize, insert: bool) -> Option<usize> {
        let lines: Vec<&[u8]> = bytes.split(|&c| c == b'\n').collect();
        let cino = cindent::CinOptions::parse(&self.format.cinoptions, self.indent.shift_width());
        let e = self.c_env(cino);
        cindent::line_indent(&lines, row.min(lines.len() - 1), col, insert, &e)
    }

    /// Whether `'cinkeys'` re-indents line `row` of `bytes` after `key` (#1163), the cursor at byte `col`
    /// of the line just after it; `line_is_empty`: only blanks before the key (it allows the `0` keys).
    #[must_use]
    pub fn c_key_reindents(
        &self,
        bytes: &[u8],
        row: usize,
        col: usize,
        key: CinKey,
        line_is_empty: bool,
    ) -> bool {
        let lines: Vec<&[u8]> = bytes.split(|&c| c == b'\n').collect();
        let cino = cindent::CinOptions::parse(&self.format.cinoptions, self.indent.shift_width());
        let e = self.c_env(cino);
        cindent::key_reindents(
            &lines,
            row.min(lines.len() - 1),
            col,
            key,
            line_is_empty,
            &e,
        )
    }

    fn c_env(&self, cino: cindent::CinOptions) -> cindent::CEnv<'_> {
        cindent::CEnv {
            ts: self.indent.tabs(),
            cino,
            cinwords: &self.format.cinwords,
            cinscopedecls: &self.format.cinscopedecls,
            cinkeys: &self.format.cinkeys,
            comments: &self.format.comments,
        }
    }

    /// This View's scroll position (first visible buffer row). Maintained by the frontend.
    #[must_use]
    pub fn top(&self) -> usize {
        self.top
    }

    /// Set this View's scroll position (frontend viewport pass).
    pub fn set_top(&mut self, top: usize) {
        self.top = top;
    }

    /// `'formatprg'`: the program `gq` filters through (empty: format internally, #618).
    #[must_use]
    pub fn formatprg(&self) -> &str {
        &self.format.prg
    }

    /// `'equalprg'`: the program `=` filters through (empty: reindent internally, #617).
    #[must_use]
    pub fn equalprg(&self) -> &str {
        &self.format.equalprg
    }

    /// Whether `cmd` (`o`, `O`, an Insert `<CR>`) on `bytes` opens a line that continues a comment under
    /// `'formatoptions'` `o`/`r` (#618). A frontend that seeds an opened line's indent from a syntax tree
    /// leaves such an open to the core.
    #[must_use]
    pub fn opens_comment(&self, bytes: &[u8], cmd: &Command) -> bool {
        planner::open_comment_line_in(self, bytes, cmd).is_some()
    }

    /// Set the columns of this View's text area (the window width less its gutter), which
    /// `'wrapmargin'` and a `gq` without `'textwidth'` measure against (frontend layout pass).
    pub fn set_text_cols(&mut self, cols: usize) {
        self.text_cols = cols.max(1);
    }

    /// The width text wraps at (nvim's `comp_textwidth`): `'textwidth'`, else the text area less
    /// `'wrapmargin'` when that is set, else `0` (no auto-wrap). `force` is `gq`/`gw`, which wraps at one
    /// column short of the text area (at most 79) when neither is set.
    pub(crate) fn text_width_for(&self, force: bool) -> usize {
        let mut tw = self.text_width;
        if tw == 0 && self.format.wrapmargin != 0 {
            let cols = i64::try_from(self.text_cols).unwrap_or(i64::MAX);
            tw = usize::try_from(cols.saturating_sub(self.format.wrapmargin)).unwrap_or(0);
        }
        if force && tw == 0 {
            tw = self.text_cols.saturating_sub(1).min(79);
        }
        tw
    }

    /// This window's own `'scrolloff'`, or `None` when it follows the global value.
    #[must_use]
    pub fn scrolloff_local(&self) -> Option<i64> {
        self.scrolloff
    }

    /// Set (or, with `None`, clear) this window's own `'scrolloff'` (`:setlocal so`, #1151).
    pub fn set_scrolloff_local(&mut self, so: Option<i64>) {
        self.scrolloff = so;
    }

    /// The first displayed screen column when `'wrap'` is off (#615).
    #[must_use]
    pub fn leftcol(&self) -> usize {
        self.leftcol
    }

    /// Set the first displayed screen column (the frontend's horizontal scroll, #615).
    pub fn set_leftcol(&mut self, col: usize) {
        self.leftcol = col;
    }

    /// The virtual columns of the top line scrolled out of the window (nvim's `w_skipcol`, #615).
    #[must_use]
    pub fn skipcol(&self) -> usize {
        self.skipcol
    }

    /// Set the top line's skipped virtual columns (the frontend's screen-row scroll, #615).
    pub fn set_skipcol(&mut self, col: usize) {
        self.skipcol = col;
    }

    /// The wanted column (Vim's `curswant`) as a display column, `usize::MAX` after `$`: read by the
    /// frontend's screen-line motions (`gj`, `gk`, #615).
    #[must_use]
    pub fn curswant(&self) -> usize {
        self.curswant.get()
    }

    /// This window's display options (#615, #616).
    #[must_use]
    pub fn window_options(&self) -> &crate::WindowOptions {
        &self.window
    }

    /// Replace this window's display options.
    pub fn set_window_options(&mut self, opts: crate::WindowOptions) {
        self.window = opts;
    }

    /// The `CTRL-D`/`CTRL-U` amount for a window `height` rows high (Vim's 'scroll'): the value a count last
    /// set at this height, else half the height (at least 1).
    #[must_use]
    pub fn scroll(&self, height: usize) -> usize {
        match self.scroll {
            Some((lines, h)) if h == height => lines,
            _ => (height / 2).max(1),
        }
    }

    /// Set Vim's 'scroll' for a window `height` rows high (a count typed before `CTRL-D`/`CTRL-U`).
    pub fn set_scroll(&mut self, lines: usize, height: usize) {
        self.scroll = Some((lines, height));
    }

    /// The highlighted byte range `[start, end)` of the current charwise/linewise Visual selection over
    /// `bytes`, or `None` in Normal/Insert and for a blockwise selection (a rectangle is not one range —
    /// use [`View::block_spans`]). See [`EditorState::selection_span`] for the full contract.
    #[must_use]
    pub fn selection_span(&self, bytes: &[u8]) -> Option<(BytePos, BytePos)> {
        match self.mode.selection()? {
            SelectKind::Blockwise => None,
            kind => {
                let (s, e) = selection_range(
                    bytes,
                    self.anchor?,
                    self.cursor,
                    kind == SelectKind::Linewise,
                );
                Some((BytePos(s), BytePos(e)))
            }
        }
    }

    /// The per-row highlighted byte ranges of the current BLOCKWISE selection over `bytes`, or `None`
    /// when the selection is not blockwise. See [`EditorState::block_spans`].
    #[must_use]
    pub fn block_spans(&self, bytes: &[u8]) -> Option<Vec<(BytePos, BytePos)>> {
        match self.mode.selection()? {
            SelectKind::Blockwise => Some(
                block_geom(
                    bytes,
                    self.anchor?,
                    self.cursor,
                    self.indent.tabs(),
                    self.curswant == MAXCOL,
                )
                .spans()
                .into_iter()
                .map(|(s, e)| (BytePos(s), BytePos(e)))
                .collect(),
            ),
            _ => None,
        }
    }
}
