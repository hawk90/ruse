//! Keys run inside a command: `:normal`, `:g` with a Normal or Ex payload, and the frames they paint
//! without scrolling. Split from `session.rs` (#1423).

use super::{
    line_index, note_ex, place_cursor, report, window_rects, Command, ExCmd, FrameCtx, GroupKind,
    KeyCode, KeyEvent, KeyModifiers, Mode, PaneText, Prefix, Rect, Session, Silence,
    TransactionOrigin, WinView,
};
#[cfg(test)]
use super::{persist, PathBuf, SessionInit, TermInfo, Workspace};

impl Session {
    /// Each window's display as its options and its scroll position are now (#615), for a frame that does
    /// not scroll (a key a `:normal` feeds).
    pub(super) fn win_views(&self, rects: &[Rect]) -> Vec<WinView> {
        let has_signs = !self
            .lsp
            .diagnostics_for(self.ed.ws.focused_buffer())
            .is_empty();
        rects
            .iter()
            .enumerate()
            .take(self.ed.ws.window_count())
            .map(|(i, rect)| {
                let p = self.ed.ws.pane(i);
                let mut ix = line_index::LineIndex::default();
                let idx = if p.view.doc() == self.ed.ws.focused_buffer() {
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
                let folds = if i == self.ed.ws.focus() {
                    crate::folds::for_buffer(&self.folds, self.ed.ws.focused_buffer())
                } else {
                    &[]
                };
                WinView::resolve(
                    p.view,
                    &self.ui_opts.display,
                    rect.w as usize,
                    rect.h as usize,
                    text.line_count(),
                    i == self.ed.ws.focus() && has_signs,
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
        let pane = self.ed.ws.focused();
        if !matches!(
            pane.view.mode(),
            Mode::Insert | Mode::Replace | Mode::VirtualReplace
        ) {
            return;
        }
        let line = ruse_core::pos::line_of(pane.doc.bytes(), pane.view.cursor());
        if let Some(fv) = self.folds.get_mut(&self.ed.ws.focused_buffer()) {
            crate::folds::open_at(fv, line);
        }
    }

    pub(super) fn nested_frame(&mut self, frame: &FrameCtx) -> FrameCtx {
        let (revision, snapshot) = {
            let f = self.ed.ws.focused();
            (f.doc.revision(), f.doc.text_arc())
        };
        self.line_idx
            .refresh_for(self.ed.ws.focused_buffer(), revision, &snapshot);
        let rects = window_rects(
            frame.cols,
            frame.text_rows,
            self.ed.ws.window_count(),
            self.ed.ws.split_dir(),
        );
        let wins = self.win_views(&rects);
        FrameCtx {
            revision,
            snapshot,
            rects,
            wins,
            cols: frame.cols,
            text_rows: frame.text_rows,
            rows: frame.rows,
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
        let status_before = self.ed.status.serial();
        self.route(key, true, &f);
        self.open_folds_in_insert();
        if self.failed_since(status_before) {
            self.macros.flush_replay();
            return true;
        }
        while !self.ed.quit {
            if let Some(line) = self.macros.next_line_run() {
                let b = self.ed.ws.focused().doc.bytes();
                let lines =
                    ruse_core::pos::count_newlines(b) + usize::from(b.last() != Some(&b'\n'));
                let at = ruse_core::pos::nth_line_start(b, line.min(lines.saturating_sub(1))).0;
                place_cursor(&mut self.ed.ws, &mut self.ed.recorded, at);
            }
            if ruse_core::interrupt::breakcheck() {
                self.macros.flush_replay();
                return true;
            }
            let Some(k) = self.macros.pop_replay() else {
                break;
            };
            let f = self.nested_frame(frame);
            let status_before = self.ed.status.serial();
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
        let origin = self.ed.ws.origin();
        if from_macro {
            self.ed.ws.set_origin(TransactionOrigin::Macro);
        }
        self.route(key, from_macro, frame);
        self.ed.ws.set_origin(origin);
    }

    /// Whether the key just routed failed (Vim beeps or gives an error), clearing the beep: nvim's
    /// `beep_flush`/`emsg` then flush the keys that were not typed (`flush_buffers(FLUSH_MINIMAL)`) — the rest
    /// of a macro replay, of a `:normal` payload, of a `:g` pass. Typed keys go on (#1085).
    pub(super) fn failed_since(&mut self, status_before: u64) -> bool {
        let error = self.ed.status.serial() != status_before && self.ed.status.is_error();
        self.ed.ws.take_beep() || error
    }

    /// One `:normal` pass of `events` at the cursor, then the implicit `<Esc>` that aborts an unfinished
    /// command and ends Insert (`:help :normal`). Every key takes the typed-key path, so the session's own
    /// resolution (`*`, `gd`, `H`/`M`/`L`, `&`, `@`, a nested `:` line, …) applies, which `engine.feed`
    /// alone skipped (#689).
    ///
    /// The `<Esc>` is only for a command the keys left unfinished: nvim's `exec_normal` runs commands while
    /// keys remain, and only a key read after the last one is `<Esc>` (`vgetorpeek` under
    /// `ex_normal_busy`). A Visual or Select mode the keys started, at a finished command, stays (#1307):
    /// `:normal vl` ends in Visual, and a range's next line extends it.
    pub(super) fn normal_pass(&mut self, events: &[KeyEvent], frame: &FrameCtx) {
        for &ev in events {
            if self.ed.quit {
                return;
            }
            if self.nested_key(ev, frame) {
                break;
            }
        }
        if self.ed.quit {
            return;
        }
        let selecting = |m: Mode| matches!(m, Mode::Visual { .. } | Mode::Select { .. });
        let session_idle = matches!(self.prefix, Prefix::None)
            && !self.macros.awaits_register()
            && !self.overlay_open();
        let in_visual = selecting(self.ed.ws.focused().view.mode());
        // A trailing `"x` is a finished command whose register goes unused (`:normal vl"a` stays in
        // Visual).
        if in_visual && session_idle && self.engine.drop_bare_register() {
            self.ed.run(Command::SetRegister(None));
        }
        if !(in_visual && session_idle && self.engine.at_clean_boundary()) {
            self.nested_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), frame);
        }
        // The `:normal` saved and restores the keys around it, so its failure does not flush them.
        self.ed.ws.take_beep();
        // The next line's column-0 placement relies on Normal or a Visual still being made (an Esc ends
        // the other modes, but not every state a key can leave behind).
        let mode = self.ed.ws.focused().view.mode();
        if mode != Mode::Normal && !selecting(mode) {
            let Session { .. } = self;
            self.ed.run(Command::EnterNormal);
        }
    }

    /// Run `body` as a `:normal`: refuse past [`MAX_NORMAL_DEPTH`] nested levels (E192), set aside the
    /// replay queue and an open `gQ` line (#807) so the keys reach Normal mode and the outer keys resume
    /// after, and record every change as one undo step (#852: Vim does not sync undo between keys that
    /// were not typed).
    pub(super) fn as_normal(&mut self, kind: GroupKind, body: impl FnOnce(&mut Session)) {
        if self.normal_depth >= MAX_NORMAL_DEPTH {
            self.ed
                .status
                .error("E192: Recursive use of :normal too deep");
            return;
        }
        self.normal_depth += 1;
        let insert_return = self.leave_insert_for_normal();
        let replay = self.macros.suspend_replay();
        let line = self.engine.suspend_cmdline();
        let doc = self.ed.ws.focused_buffer();
        let opened = !self.ed.ws.focused().doc.in_undo_block();
        if opened {
            self.ed.ws.focused_doc_mut().begin_undo_block();
        }
        // The group a trace replays as one undo step, as this block makes it live (RFC-0020 R3).
        note_ex(ExCmd::Begin(kind), &mut self.ed.recorded);
        body(self);
        note_ex(ExCmd::End, &mut self.ed.recorded);
        if opened {
            if let Some(d) = self.ed.ws.document_mut(doc) {
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
        if self.ed.ws.focused().view.mode() != Mode::Insert {
            return None;
        }
        let b = self.ed.ws.focused().doc.bytes();
        let cur = self.ed.ws.focused().view.cursor().0.min(b.len());
        let line = ruse_core::pos::line_of(b, ruse_core::BytePos(cur));
        let start = ruse_core::pos::nth_line_start(b, line).0.min(b.len());
        let at_eol = cur > start && b.get(cur).is_none_or(|&c| c == b'\n');
        let place = if at_eol { prev_char_start(b, cur) } else { cur };
        let Session { ed, .. } = self;
        ed.run(Command::EnterNormal);
        place_cursor(&mut ed.ws, &mut ed.recorded, place);
        Some(InsertReturn { line, at_eol })
    }

    /// Resume the Insert a [`Self::leave_insert_for_normal`] left: after the last char when the caret was
    /// past the end of the line and is still on that line's last char (Vim `edit()` with `ins_at_eol`),
    /// else at the caret. Nothing when the keys quit or left a mode other than Normal.
    pub(super) fn resume_insert_after_normal(&mut self, ret: InsertReturn) {
        if self.ed.quit || self.ed.ws.focused().view.mode() != Mode::Normal {
            return;
        }
        let b = self.ed.ws.focused().doc.bytes();
        let cur = self.ed.ws.focused().view.cursor().0.min(b.len());
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
        self.ed.run(cmd);
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
                let (line1, line2) = crate::app::dispatch::resolve_normal_range(&s.ed.ws, range);
                for lnum in line1..=line2 {
                    if s.ed.quit || ruse_core::interrupt::interrupted() {
                        break;
                    }
                    let b = s.ed.ws.focused().doc.bytes();
                    let last = crate::app::dispatch::line_count(b).max(1);
                    let off = ruse_core::pos::nth_line_start(b, lnum.min(last) - 1).0;
                    place_cursor(&mut s.ed.ws, &mut s.ed.recorded, off);
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
        self.ed
            .status
            .info(format!("{} lines changed", marks.len()));
    }

    /// Execute `:[range]g/pat/{cmd}` for any command but the ones core runs in one pass (#1245): `cmd`, `|`
    /// and all, runs through the Ex dispatch once per marked line that still exists, with the cursor at
    /// the start of that line and so the command's default range there (Vim's `global_exe`). The first
    /// command that fails ends the `:g` (Vim's `emsg` bumps `global_busy`); one a `:silent!` hid does not.
    /// The whole `:g` is one undo step, and the trace records it as a group of the commands it ran.
    pub(super) fn ex_global_ex(&mut self, marks: &[usize], cmd: &str, frame: &FrameCtx) {
        let pieces = crate::input::split_bar(cmd);
        let lines = |s: &Session| crate::app::dispatch::line_count(s.ed.ws.focused().doc.bytes());
        let before = lines(self);
        let mut last = None;
        self.over_global_marks(marks, |s| {
            s.run_ex_pieces(pieces.clone(), frame);
            if !s.ed.status.is_empty() {
                last = Some(s.ed.status.shown());
            }
            !(s.ed.status.is_error() || s.swap.is_some() || s.confirm.is_some())
        });
        if self.ed.status.is_error() || self.swap.is_some() || self.confirm.is_some() {
            return;
        }
        // Vim reports the lines the commands added or removed (`msgmore`, past 'report' = 2), else the
        // message of the last command that left one.
        let after = lines(self);
        // The commands ran with their reports kept out of the history (as nvim's `global_busy` holds back
        // `msgmore`); the one report the `:g` gives is kept.
        match report::line_delta(before, after, self.ui_opts.report) {
            Some(msg) => self.ed.status.info(msg),
            None => match last {
                Some(msg) => self.ed.status.give(msg),
                None => self.ed.status.clear(),
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
        let doc_id = self.ed.ws.focused_buffer();
        // Each line is held by two anchors: its start (pushed right by text inserted before it) and the
        // start of the next line (held left). A deleted line collapses them together.
        let anchors: Vec<_> = {
            let doc = self.ed.ws.focused_doc_mut();
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
        let hushed = self.ed.status.hush(Silence::Messages);
        self.as_normal(GroupKind::Global, |s| {
            for &(start, end, empty_buffer) in &anchors {
                if s.ed.quit
                    || s.ed.ws.focused_buffer() != doc_id
                    || ruse_core::interrupt::interrupted()
                {
                    break;
                }
                if visited.len() >= batch {
                    s.ed.ws.focused_doc_mut().remove_anchors(visited.drain(..));
                }
                visited.extend([start, end]);
                let doc = s.ed.ws.focused().doc;
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
                place_cursor(&mut s.ed.ws, &mut s.ed.recorded, a);
                if !each(s) {
                    break;
                }
            }
        });
        self.global_busy = busy;
        self.ed.status.unhush(hushed);
        if let Some(doc) = self.ed.ws.document_mut(doc_id) {
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
        std::mem::swap(&mut s.ed.ws, ws);
        let frame = s.pre_frame(80, 24);
        s.execute_ex_one(line.to_string(), &frame);
        std::mem::swap(&mut s.ed.ws, ws);
        s
    }
}

/// How deep `:normal` may nest (`:normal :normal …`, or a macro that runs `:normal @a` on itself) before it
/// fails with E192. Vim's limit is 'maxmapdepth' (1000); each level here is a few session frames on the
/// Rust stack, so ruse stops earlier.
const MAX_NORMAL_DEPTH: usize = 20;

/// Where Insert resumes after a `:normal` run from `i_CTRL-O` (#1097): the caret's line, and whether it
/// sat past the end of that line.
pub(super) struct InsertReturn {
    line: usize,
    at_eol: bool,
}

/// The start of the UTF-8 char before byte `at` (`0` at the start).
fn prev_char_start(b: &[u8], at: usize) -> usize {
    let mut i = at.saturating_sub(1);
    while i > 0 && b.get(i).is_some_and(|&c| c & 0xC0 == 0x80) {
        i -= 1;
    }
    i
}

/// The start of the UTF-8 char after the one at byte `at`.
fn next_char_start(b: &[u8], at: usize) -> usize {
    let mut i = at + 1;
    while b.get(i).is_some_and(|&c| c & 0xC0 == 0x80) {
        i += 1;
    }
    i
}

/// The offsets of every `\n` in `b`, in order.
fn newline_offsets(b: &[u8]) -> impl Iterator<Item = usize> + '_ {
    b.iter()
        .enumerate()
        .filter(|&(_, &c)| c == b'\n')
        .map(|(i, _)| i)
}
