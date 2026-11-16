//! Keys run inside a command: `:normal`, `:g` with a Normal or Ex payload, and the frames they paint
//! without scrolling. Split from `session.rs` (#1423).

use super::*;

impl Session {
    /// Each window's display as its options and its scroll position are now (#615), for a frame that does
    /// not scroll (a key a `:normal` feeds).
    pub(super) fn win_views(&self, rects: &[Rect]) -> Vec<WinView> {
        let has_signs = !self
            .lsp
            .diagnostics_for(self.ws.focused_buffer())
            .is_empty();
        rects
            .iter()
            .enumerate()
            .take(self.ws.window_count())
            .map(|(i, rect)| {
                let p = self.ws.pane(i);
                let mut ix = line_index::LineIndex::default();
                let idx = if p.view.doc() == self.ws.focused_buffer() {
                    &self.line_idx
                } else if let Some(idx) = self.pane_idx.get(p.view.doc(), p.doc.revision()) {
                    idx
                } else {
                    ix.refresh_for(p.view.doc(), p.doc.revision(), p.doc.bytes());
                    &ix
                };
                let text = PaneText {
                    bytes: p.doc.bytes(),
                    idx,
                };
                let folds = if i == self.ws.focus() {
                    self.folds
                        .get(&self.ws.focused_buffer())
                        .map_or(&[][..], Vec::as_slice)
                } else {
                    &[]
                };
                WinView::resolve(
                    p.view,
                    &self.ui_opts.display,
                    rect.w as usize,
                    rect.h as usize,
                    text.line_count(),
                    i == self.ws.focus() && has_signs,
                )
                .with_folds(folds)
            })
            .collect()
    }

    /// A frame for a key a `:normal` feeds: the buffer snapshot, line index and pane geometry as they are
    /// now (earlier keys of the same `:normal` may have edited, split or switched), with `frame`'s size.
    /// Insert mode opens the folds holding the cursor line (nvim's `edit` calls `foldOpenCursor`), also
    /// for keys run between frames (`:normal`, a `:g` of them).
    pub(super) fn open_folds_in_insert(&mut self) {
        let pane = self.ws.focused();
        if !matches!(
            pane.view.mode(),
            Mode::Insert | Mode::Replace | Mode::VirtualReplace
        ) {
            return;
        }
        let line = ruse_core::pos::line_of(pane.doc.bytes(), pane.view.cursor());
        if let Some(fv) = self.folds.get_mut(&self.ws.focused_buffer()) {
            crate::folds::open_at(fv, line);
        }
    }

    pub(super) fn nested_frame(&mut self, frame: &FrameCtx) -> FrameCtx {
        let (revision, snapshot) = {
            let f = self.ws.focused();
            (f.doc.revision(), f.doc.text_arc())
        };
        self.line_idx
            .refresh_for(self.ws.focused_buffer(), revision, &snapshot);
        let rects = window_rects(
            frame.cols,
            frame.text_rows,
            self.ws.window_count(),
            self.ws.split_dir(),
        );
        let wins = self.win_views(&rects);
        FrameCtx {
            revision,
            snapshot,
            rects,
            wins,
            cols: frame.cols,
            text_rows: frame.text_rows,
        }
    }

    /// Feed one key of a `:normal` through the whole session key path (#689), then run any replay it
    /// started (`@a`, `Q`, a linewise-Visual `@`) to completion, as Vim executes it inside the `:normal`.
    ///
    /// Returns whether a command failed: the rest of the replay it started is dropped then, and the caller
    /// drops the rest of its `:normal` keys too (#1085).
    pub(super) fn nested_key(&mut self, key: KeyEvent, frame: &FrameCtx) -> bool {
        // CTRL-C stops a `:normal` (and the `:g` or range driving it) between keys.
        if ruse_core::interrupt::breakcheck() {
            self.macros.flush_replay();
            return true;
        }
        let f = self.nested_frame(frame);
        let status_before = self.status.serial();
        self.route(key, true, &f);
        self.open_folds_in_insert();
        if self.failed_since(status_before) {
            self.macros.flush_replay();
            return true;
        }
        while !self.quit {
            if let Some(line) = self.macros.next_line_run() {
                let b = self.ws.focused().doc.bytes();
                let lines =
                    ruse_core::pos::count_newlines(b) + usize::from(b.last() != Some(&b'\n'));
                let at = ruse_core::pos::nth_line_start(b, line.min(lines.saturating_sub(1))).0;
                place_cursor(&mut self.ws, &mut self.recorded, at);
            }
            if ruse_core::interrupt::breakcheck() {
                self.macros.flush_replay();
                return true;
            }
            let Some(k) = self.macros.pop_replay() else {
                break;
            };
            let f = self.nested_frame(frame);
            let status_before = self.status.serial();
            self.route_as(k, true, &f);
            self.open_folds_in_insert();
            if self.failed_since(status_before) {
                self.macros.flush_replay();
                return true;
            }
        }
        false
    }

    /// [`Session::route`] a key, its edits recorded under [`TransactionOrigin::Macro`] when it came from a
    /// macro replay's queue (`@{reg}`, `@:`, `Q`), else under the current origin (INV-ORIGIN, #880).
    pub(super) fn route_as(&mut self, key: KeyEvent, from_macro: bool, frame: &FrameCtx) {
        let origin = self.ws.origin();
        if from_macro {
            self.ws.set_origin(TransactionOrigin::Macro);
        }
        self.route(key, from_macro, frame);
        self.ws.set_origin(origin);
    }

    /// Whether the key just routed failed (Vim beeps or gives an error), clearing the beep: nvim's
    /// `beep_flush`/`emsg` then flush the keys that were not typed (`flush_buffers(FLUSH_MINIMAL)`) — the rest
    /// of a macro replay, of a `:normal` payload, of a `:g` pass. Typed keys go on (#1085).
    pub(super) fn failed_since(&mut self, status_before: u64) -> bool {
        let error = self.status.serial() != status_before && self.status.is_error();
        self.ws.take_beep() || error
    }

    /// One `:normal` pass of `events` at the cursor, then the implicit `<Esc>` that aborts an unfinished
    /// command and ends Insert (`:help :normal`). Every key takes the typed-key path, so the session's own
    /// resolution (`*`, `gd`, `H`/`M`/`L`, `&`, `@`, a nested `:` line, …) applies, which `engine.feed`
    /// alone skipped (#689).
    pub(super) fn normal_pass(&mut self, events: &[KeyEvent], frame: &FrameCtx) {
        for &ev in events {
            if self.quit {
                return;
            }
            if self.nested_key(ev, frame) {
                break;
            }
        }
        if self.quit {
            return;
        }
        self.nested_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), frame);
        // The `:normal` saved and restores the keys around it, so its failure does not flush them.
        self.ws.take_beep();
        // The next line's column-0 placement relies on Normal (an Esc leaves Visual too, but not every
        // state a key can leave behind).
        if self.ws.focused().view.mode() != Mode::Normal {
            let Session { .. } = self;
            self.run_cmd(Command::EnterNormal);
        }
    }

    /// Run `body` as a `:normal`: refuse past [`MAX_NORMAL_DEPTH`] nested levels (E192), set aside the
    /// replay queue and an open `gQ` line (#807) so the keys reach Normal mode and the outer keys resume
    /// after, and record every change as one undo step (#852: Vim does not sync undo between keys that
    /// were not typed).
    pub(super) fn as_normal(&mut self, kind: GroupKind, body: impl FnOnce(&mut Session)) {
        if self.normal_depth >= MAX_NORMAL_DEPTH {
            self.status.error("E192: Recursive use of :normal too deep");
            return;
        }
        self.normal_depth += 1;
        let insert_return = self.leave_insert_for_normal();
        let replay = self.macros.suspend_replay();
        let line = self.engine.suspend_cmdline();
        let doc = self.ws.focused_buffer();
        let opened = !self.ws.focused().doc.in_undo_block();
        if opened {
            self.ws.focused_doc_mut().begin_undo_block();
        }
        // The group a trace replays as one undo step, as this block makes it live (RFC-0020 R3).
        note_ex(ExCmd::Begin(kind), &mut self.recorded);
        body(self);
        note_ex(ExCmd::End, &mut self.recorded);
        if opened {
            if let Some(d) = self.ws.document_mut(doc) {
                d.end_undo_block();
            }
        }
        self.engine.resume_cmdline(line);
        self.macros.resume_replay(replay);
        if let Some(ret) = insert_return {
            self.resume_insert_after_normal(ret);
        }
        self.normal_depth -= 1;
    }

    /// A `:normal` typed from `i_CTRL-O` (the core mode still reads Insert) runs its keys in NORMAL mode,
    /// as Vim's `ex_normal` clears `restart_edit` (#1097): leave Insert with the caret where it was — on
    /// the last char when it sat past the end of the line, as `i_CTRL-O` does — and return where Insert
    /// resumes. `None` when not in Insert.
    pub(super) fn leave_insert_for_normal(&mut self) -> Option<InsertReturn> {
        if self.ws.focused().view.mode() != Mode::Insert {
            return None;
        }
        let b = self.ws.focused().doc.bytes();
        let cur = self.ws.focused().view.cursor().0.min(b.len());
        let line = ruse_core::pos::line_of(b, ruse_core::BytePos(cur));
        let start = ruse_core::pos::nth_line_start(b, line).0.min(b.len());
        let at_eol = cur > start && b.get(cur).is_none_or(|&c| c == b'\n');
        let place = if at_eol { prev_char_start(b, cur) } else { cur };
        let Session {
            ws,
            files,
            recorded,
            status,
            quit,
            ..
        } = self;
        run_cmd(Command::EnterNormal, ws, files, recorded, status, quit);
        place_cursor(ws, recorded, place);
        Some(InsertReturn { line, at_eol })
    }

    /// Resume the Insert a [`Self::leave_insert_for_normal`] left: after the last char when the caret was
    /// past the end of the line and is still on that line's last char (Vim `edit()` with `ins_at_eol`),
    /// else at the caret. Nothing when the keys quit or left a mode other than Normal.
    pub(super) fn resume_insert_after_normal(&mut self, ret: InsertReturn) {
        if self.quit || self.ws.focused().view.mode() != Mode::Normal {
            return;
        }
        let b = self.ws.focused().doc.bytes();
        let cur = self.ws.focused().view.cursor().0.min(b.len());
        let on_last = b.get(cur).is_some_and(|&c| c != b'\n')
            && b.get(next_char_start(b, cur)).is_none_or(|&c| c == b'\n');
        let cmd = if ret.at_eol
            && on_last
            && ruse_core::pos::line_of(b, ruse_core::BytePos(cur)) == ret.line
        {
            Command::EnterInsertAfter
        } else {
            Command::EnterInsert
        };
        let Session { .. } = self;
        self.run_cmd(cmd);
        self.engine.resume_insert_after_one_shot();
    }

    /// Execute `:[range]normal[!] {keys}` (`:help :normal`). With no range it runs ONCE at the cursor
    /// (column kept); with a range it runs once per line, cursor at column 0. As in Vim (nvim v0.12.4), the
    /// loop runs over the ORIGINAL `[line1, line2]` and each step targets `min(lnum, last_line)` of the
    /// CURRENT buffer, so `:%normal dd` deletes every line. `bang` (no user mappings) is inert: ruse has
    /// none yet.
    pub(super) fn ex_normal(
        &mut self,
        range: Option<ruse_core::SubRange>,
        keys: &str,
        frame: &FrameCtx,
    ) {
        let events = crate::keys::parse_normal_keys(keys);
        self.as_normal(GroupKind::Normal, |s| match range {
            None => s.normal_pass(&events, frame),
            Some(range) => {
                let (line1, line2) = crate::app::dispatch::resolve_normal_range(&s.ws, range);
                for lnum in line1..=line2 {
                    if s.quit || ruse_core::interrupt::interrupted() {
                        break;
                    }
                    let b = s.ws.focused().doc.bytes();
                    let last = crate::app::dispatch::line_count(b).max(1);
                    let off = ruse_core::pos::nth_line_start(b, lnum.min(last) - 1).0;
                    place_cursor(&mut s.ws, &mut s.recorded, off);
                    s.normal_pass(&events, frame);
                }
            }
        });
    }

    /// Execute `:[range]g/pat/normal {keys}` (`:help :g`) over the lines PASS 1 marked: `{keys}` run as a
    /// `:normal` at column 0 of each marked line that still exists ([`Self::over_global_marks`]).
    pub(super) fn ex_global_normal(&mut self, marks: &[usize], keys: &str, frame: &FrameCtx) {
        let events = crate::keys::parse_normal_keys(keys);
        self.over_global_marks(marks, |s| {
            s.normal_pass(&events, frame);
            true
        });
        self.status.info(format!("{} lines changed", marks.len()));
    }

    /// Execute `:[range]g/pat/{cmd}` for any command but the ones core runs in one pass (#1245): `cmd`, `|`
    /// and all, runs through the Ex dispatch once per marked line that still exists, with the cursor at
    /// the start of that line and so the command's default range there (Vim's `global_exe`). The first
    /// command that fails ends the `:g` (Vim's `emsg` bumps `global_busy`); one a `:silent!` hid does not.
    /// The whole `:g` is one undo step, and the trace records it as a group of the commands it ran.
    pub(super) fn ex_global_ex(&mut self, marks: &[usize], cmd: &str, frame: &FrameCtx) {
        let pieces = crate::input::split_bar(cmd);
        let lines = |s: &Session| crate::app::dispatch::line_count(s.ws.focused().doc.bytes());
        let before = lines(self);
        let mut last = None;
        self.over_global_marks(marks, |s| {
            s.run_ex_pieces(pieces.clone(), frame);
            if !s.status.is_empty() {
                last = Some(s.status.shown());
            }
            !(s.status.is_error() || s.swap.is_some() || s.confirm.is_some())
        });
        if self.status.is_error() || self.swap.is_some() || self.confirm.is_some() {
            return;
        }
        // Vim reports the lines the commands added or removed (`msgmore`, past 'report' = 2), else the
        // message of the last command that left one.
        let after = lines(self);
        // The commands ran with their reports kept out of the history (as nvim's `global_busy` holds back
        // `msgmore`); the one report the `:g` gives is kept.
        match report::line_delta(before, after, self.ui_opts.report) {
            Some(msg) => self.status.info(msg),
            None => match last {
                Some(msg) => self.status.give(msg),
                None => self.status.clear(),
            },
        }
    }

    /// PASS 2 of a `:g` (`:help :g`): run `each` once per line PASS 1 marked (`marks`, 1-based) that still
    /// exists, the cursor placed at its start, until `each` returns `false`. The marks are anchors that move
    /// with the edits (#853, INV-ANCHOR), so a mark follows its line wherever earlier passes moved it, and a
    /// line deleted (or joined into another) is skipped, as Vim's line marks are. Lines the commands insert
    /// are never marked. The pass runs as a `:normal` does (one undo step, a trace group) with
    /// [`Session::global_busy`] set.
    pub(super) fn over_global_marks(
        &mut self,
        marks: &[usize],
        mut each: impl FnMut(&mut Session) -> bool,
    ) {
        use ruse_core::{AnchorPolicy, Bias, BytePos};
        let doc_id = self.ws.focused_buffer();
        // Each line is held by two anchors: its start (pushed right by text inserted before it) and the
        // start of the next line (held left). A deleted line collapses them together.
        let anchors: Vec<_> = {
            let doc = self.ws.focused_doc_mut();
            // Every line start in one pass: a start per mark looked up by a scan from the top made a `:g`
            // over many lines quadratic before it ran a single pass.
            let b = doc.bytes();
            let starts: Vec<usize> = std::iter::once(0)
                .chain(newline_offsets(b).map(|i| i + 1))
                .collect();
            let start = |l: usize| starts.get(l).copied().unwrap_or(b.len());
            let spans: Vec<(usize, usize)> =
                marks.iter().map(|&l| (start(l - 1), start(l))).collect();
            // One index rebuild for all of them, not one per anchor.
            let ids = doc.create_anchors(spans.iter().flat_map(|&(s, e)| {
                [
                    (BytePos(s), Bias::After, AnchorPolicy::Clamp),
                    (BytePos(e), Bias::Before, AnchorPolicy::Clamp),
                ]
            }));
            spans
                .iter()
                .zip(ids.as_chunks::<2>().0)
                .map(|(&(s, e), &[a, z])| (a, z, s == e))
                .collect()
        };
        let busy = std::mem::replace(&mut self.global_busy, true);
        // The marks already visited are dropped as the pass goes (#1257). A deleted line's two anchors
        // collapse where it was, which is where the next command edits, so kept they would all be carried
        // through every later edit. Each removal re-sorts the store at the next edit, so they go in
        // batches of about the square root of the anchor count.
        let batch = anchors.len().isqrt().max(32) * 2;
        let mut visited: Vec<ruse_core::AnchorId> = Vec::with_capacity(batch);
        // The commands' own reports stay out of `:messages`; the `:g` gives its one report after (nvim's
        // `global_busy` holds back `msgmore`). Errors are kept.
        let hushed = self.status.hush(Silence::Messages);
        self.as_normal(GroupKind::Global, |s| {
            for &(start, end, empty_buffer) in &anchors {
                if s.quit || s.ws.focused_buffer() != doc_id || ruse_core::interrupt::interrupted()
                {
                    break;
                }
                if visited.len() >= batch {
                    s.ws.focused_doc_mut().remove_anchors(visited.drain(..));
                }
                visited.extend([start, end]);
                let doc = s.ws.focused().doc;
                let (Some(a), Some(z)) = (doc.resolve_anchor(start), doc.resolve_anchor(end))
                else {
                    continue;
                };
                let (a, z) = (a.offset.0, z.offset.0);
                let b = doc.bytes();
                let at_line_start = a == 0 || b.get(a - 1) == Some(&b'\n');
                if !(a < z || empty_buffer) || !at_line_start {
                    continue; // the line was deleted, or joined into the one before it
                }
                place_cursor(&mut s.ws, &mut s.recorded, a);
                if !each(s) {
                    break;
                }
            }
        });
        self.global_busy = busy;
        self.status.unhush(hushed);
        if let Some(doc) = self.ws.document_mut(doc_id) {
            doc.remove_anchors(anchors.into_iter().flat_map(|(start, end, _)| [start, end]));
        }
    }

    /// Test seam: run ONE Ex command (`line`, no leading `:`) against `ws` through the session's Ex
    /// dispatch, as a typed `:{line}<CR>` would after the command line closes. `ws` is moved into a fresh
    /// session for the call and back out after. Returns the session for its status / quit / confirm.
    #[cfg(test)]
    pub(crate) fn ex_on(ws: &mut Workspace, line: &str) -> Session {
        let mut s = Session::new(SessionInit {
            path: None,
            fmt: persist::encoding::FileFormat::default(),
            initial: ws.focused().doc.bytes().to_vec(),
            status: String::new(),
            profile: "vim",
            term: TermInfo::default(),
            cwd: PathBuf::new(),
            trusted: false,
            rescue: false,
        });
        std::mem::swap(&mut s.ws, ws);
        let frame = s.pre_frame(80, 24);
        s.execute_ex_one(line.to_string(), &frame);
        std::mem::swap(&mut s.ws, ws);
        s
    }
}
