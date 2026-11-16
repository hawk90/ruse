//! The passes `Session::pre_frame` runs before each render (#1355): fold reconcile, the crash-rescue
//! journals, the `:s///c` follow and the per-window scroll. Each borrows the `Session` fields it uses.
//! The frame entry points themselves live here too (#1496): [`Session::pre_frame`],
//! [`Session::post_render`], the poll timeout, the status-line flags and the fold invariant check.

use super::normal_keys::focused_folds;
use super::*;

impl Session {
    /// Fold reconcile (per frame): (a) carry manual folds through the edits since the last frame by their
    /// line anchors (#819); (b) in Insert and Replace mode open the folds holding the cursor line, as nvim
    /// does when Insert starts ("the cursor line is never folded"). The cursor may rest inside a closed
    /// fold otherwise; it paints on the fold's summary row.
    pub(super) fn reconcile_folds(&mut self, revision: ruse_core::Revision, snapshot: &[u8]) {
        let Session {
            ws,
            line_idx,
            folds,
            fold_anchors,
            fold_method,
            foldlevel,
            foldenable,
            fold_rev,
            ..
        } = self;
        let buf = ws.focused_buffer();
        let indent = fold_method.get(&buf).copied() == Some(crate::input::FoldMethod::Indent);
        if indent {
            // Indent folds are DERIVED: recompute from indentation whenever the buffer content changed
            // (revision bump), then apply the foldlevel/foldenable rule. This resets any per-fold
            // za/zo/zc toggle on edit — an honest first slice; foldlevel drives closed state between
            // edits (zr/zm/zR/zM and :set foldlevel re-apply immediately, below).
            if fold_rev.get(&buf) != Some(&revision) {
                let view = &ws.focused().view;
                let mut computed =
                    crate::folds::compute_indent_folds(snapshot, view.tabs(), view.shift_width());
                let en = foldenable.get(&buf).copied().unwrap_or(true);
                let fdl = if en {
                    foldlevel.get(&buf).copied().unwrap_or(0)
                } else {
                    usize::MAX // foldenable off ⇒ nothing closed
                };
                crate::folds::apply_foldlevel(&mut computed, fdl);
                folds.insert(buf, computed);
                fold_rev.insert(buf, revision);
            }
            // Derived folds need no anchors; manual ones made before `:set fdm=indent` lose theirs.
            if let Some(mut a) = fold_anchors.remove(&buf) {
                a.release(ws.focused_doc_mut());
            }
        } else if let Some(fv) = folds.get_mut(&buf) {
            // Manual folds are NOT recomputed: their anchors carry them through the edits.
            let a = fold_anchors.entry(buf).or_default();
            a.follow(ws.focused().doc, fv, |o| line_idx.line_of(o));
            if !a.current(revision, fv) {
                a.anchor(ws.focused_doc_mut(), fv, |l| line_idx.nth_line_start(l));
            }
        }
        if matches!(
            ws.focused().view.mode(),
            Mode::Insert | Mode::Replace | Mode::VirtualReplace
        ) {
            if let Some(fv) = folds.get_mut(&buf) {
                crate::folds::open_at(fv, line_idx.line_of(ws.focused().view.cursor().0));
            }
        }
    }

    /// Crash-rescue store + recovery journal cover EVERY recoverable buffer, not just the focused one
    /// (#856): each file-backed buffer (keyed by its own path, F-007; incl. `:e`-opened ones) and the
    /// pathless start buffer. A scratch buffer (`:enew`) is skipped so its bytes never land in some
    /// file's recovery. `save()` clears a buffer's journal by path, so this stays consistent.
    pub(super) fn publish_rescue(&mut self) {
        let Session {
            ws,
            files,
            journals,
            journal_writers,
            journal_failed,
            readonly,
            unnamed_start,
            rescue,
            status,
            ..
        } = self;
        readonly.follow(files);
        if *rescue {
            let now = std::time::Instant::now();
            let mut entries = Vec::with_capacity(files.len() + 1);
            // `'noswapfile'`, `'updatecount'` 0 or Emacs's `auto-save-default` nil: no journal (#619).
            let journaling = |bf: &crate::app::dispatch::BufferFile| {
                bf.swapfile
                    && files.write.updatecount > 0
                    && files
                        .write
                        .emacs
                        .as_ref()
                        .is_none_or(|e| e.auto_save_default)
            };
            for (id, bf) in files.iter() {
                let Some(doc) = ws.document_mut(*id) else {
                    continue;
                };
                if !journaling(bf) {
                    if let Some(mut w) = journal_writers.remove(id) {
                        w.discard();
                    }
                    continue;
                }
                let Some(j) = *journals else {
                    // No state directory: journaling is off, and the first modified buffer says so once.
                    if doc.is_modified() && journal_failed.insert(*id) {
                        status.error(format!(
                            "E303: unable to write the recovery journal for \"{}\": no state directory \
                             (set XDG_STATE_HOME or HOME)",
                            bf.path.display()
                        ));
                    }
                    continue;
                };
                let w = journal_writers
                    .entry(*id)
                    .or_insert_with(|| persist::journal::Writer::new(j, &bf.path, bf.fmt));
                w.follow(j, &bf.path);
                // Drain the change log (and drop the journal of a buffer that became clean: undone back to
                // the saved text, saved, or re-read with `:e!`, #862), then sync if the policy says so.
                w.absorb(doc, bf.fmt, now);
                if w.due_every(now, files.write.updatecount) {
                    let r = w.flush(doc);
                    journal_outcome(r, *id, &bf.path, w.journal(), journal_failed, status);
                }
                entries.push(recover::Entry {
                    path: Some(bf.path.clone()),
                    journal: w.journal().to_path_buf(),
                    canonical: Some(w.doc().to_path_buf()),
                    format: bf.fmt,
                    bytes: doc.text_arc(),
                    modified: doc.is_modified(),
                    revision: doc.revision().0,
                    node: doc.history().current_seq().0,
                });
            }
            if let (Some(doc), Some(j)) = (unnamed_start.and_then(|id| ws.document(id)), *journals)
            {
                entries.push(recover::Entry {
                    path: None,
                    journal: j.unnamed(),
                    canonical: None,
                    format: persist::encoding::FileFormat::default(),
                    bytes: doc.text_arc(),
                    modified: doc.is_modified(),
                    revision: doc.revision().0,
                    node: doc.history().current_seq().0,
                });
            }
            // A buffer that is gone or lost its file (`:bd!`) lets its changes go with its journal.
            journal_writers.retain(|id, w| {
                let keep = files.contains_key(id) && ws.doc_is_live(*id);
                if !keep {
                    w.discard();
                }
                keep
            });
            recover::sync(entries);
        }
    }

    /// During a `:s///c` confirm, follow the current match so the viewport scrolls it into view.
    pub(super) fn follow_confirm(&mut self) {
        let Session {
            ws,
            confirm,
            status,
            ..
        } = self;
        if let Some(c) = &confirm {
            if let Some(s) = c.subs.get(c.idx) {
                ws.place_focused_cursor(s.start);
                status.echo(confirm_prompt(c));
            }
        }
    }

    /// Per-window viewport pass: scroll each pane so ITS cursor stays visible in ITS rectangle
    /// (F-007 acceptance #1 — independent scroll), counting the screen rows its wrapped lines take
    /// (nvim's update_topline, #615), and sideways when it does not wrap. Geometry is shared with
    /// render below.
    pub(super) fn scroll_windows(
        &mut self,
        cols: u16,
        term_rows: u16,
    ) -> (u16, Vec<Rect>, Vec<WinView>) {
        let global_so = self.scrolloff;
        let Session {
            ws,
            line_idx,
            pane_idx,
            folds,
            lsp,
            ui_opts,
            scroll_valid,
            ..
        } = self;
        let text_rows = term_rows.saturating_sub(1);
        let rects = window_rects(cols, text_rows, ws.window_count(), ws.split_dir());
        let has_signs = !lsp.diagnostics_for(ws.focused_buffer()).is_empty();
        let focus_buf = ws.focused_buffer();
        // The other buffers the windows show keep their own line index, rebuilt on their own edits only.
        for i in 0..ws.window_count() {
            let p = ws.pane(i);
            if p.view.doc() != focus_buf {
                pane_idx.refresh(p.view.doc(), p.doc.revision(), p.doc.bytes());
            }
        }
        pane_idx.retain(|d| {
            d != focus_buf && (0..ws.window_count()).any(|i| ws.pane(i).view.doc() == d)
        });
        let mut wins = Vec::with_capacity(rects.len());
        for (i, rect) in rects.iter().enumerate() {
            let pane_folds: &[crate::folds::Fold] = if i == ws.focus() {
                focused_folds(folds, ws)
            } else {
                &[]
            };
            let (win, top, leftcol, skipcol) = place_window(
                ws.pane(i),
                Window {
                    index: i,
                    focused: i == ws.focus(),
                    rect,
                    signs: has_signs,
                    folds: pane_folds,
                    global_so,
                    scroll_valid: *scroll_valid,
                },
                focus_buf,
                line_idx,
                pane_idx,
                ui_opts,
            );
            ws.set_top(i, top);
            ws.set_leftcol(i, leftcol);
            ws.set_skipcol(i, skipcol);
            // The window's text columns: 'wrapmargin' counts from its right edge, after the gutter.
            ws.set_text_cols(i, win.width);
            wins.push(win);
        }
        (text_rows, rects, wins)
    }
}

/// One window of the frame: where it is and what its gutter and folds show.
struct Window<'a> {
    index: usize,
    focused: bool,
    rect: &'a Rect,
    signs: bool,
    folds: &'a [crate::folds::Fold],
    /// The global `'scrolloff'` (a window-local value overrides it).
    global_so: i64,
    /// What the last scroll command left valid, if it still holds.
    scroll_valid: Option<ScrollValid>,
}

/// Lay out one window for the frame and scroll it to its cursor: its `WinView` and the top line, left
/// column and skipped columns to store on its view.
fn place_window(
    p: ruse_core::workspace::Pane<'_>,
    w: Window<'_>,
    focus_buf: DocumentId,
    line_idx: &line_index::LineIndex,
    pane_idx: &line_index::PaneIndexes,
    ui_opts: &crate::app::set_exec::UiOptions,
) -> (WinView, usize, usize, usize) {
    let idx = if p.view.doc() == focus_buf {
        line_idx
    } else {
        pane_idx
            .get(p.view.doc(), p.doc.revision())
            .unwrap_or(line_idx)
    };
    let text = PaneText {
        bytes: p.doc.bytes(),
        idx,
    };
    let height = w.rect.h as usize;
    let mut win = WinView::resolve(
        p.view,
        &ui_opts.display,
        w.rect.w as usize,
        height,
        text.line_count(),
        w.focused && w.signs,
    )
    .with_folds(w.folds);
    let cursor = p.view.cursor().0.min(text.bytes.len());
    let cline = idx.line_of(cursor);
    let lines = PaneLines {
        text,
        win: &win,
        folds: w.folds,
        virt: &[],
        height,
    };
    let so = scrolloff_for(p.view, w.global_so);
    // The cursor's line laid out up to the cursor (a huge line costs no more), once: the
    // scroll rules below read the same column.
    let line_byte = cursor - text.start(cline);
    let (start, end, vcol) = crate::ui::window_view::cursor_place(
        &win,
        text,
        cline,
        line_byte,
        win.tab_end(p.view.mode()),
    );
    let sc = crate::scroll::Scroll {
        lines: &lines,
        height,
        so,
        sms: win.smoothscroll,
        tab_end: win.tab_end(p.view.mode()),
        memo: std::cell::Cell::new(Some((cline, line_byte, vcol))),
    };
    // The top line's skipped columns (#615): the cursor moved into them shows them again,
    // and a new top line starts unskipped.
    let mut pos = crate::scroll::ViewPos {
        top: p.view.top(),
        skipcol: if win.wrap { p.view.skipcol() } else { 0 },
        line: cline,
        byte: cursor - text.start(cline),
        curswant: 0,
    };
    pos.skipcol = sc.uncover_cursor(&pos);
    if !win.smoothscroll && pos.skipcol > 0 {
        // Without 'smoothscroll' a scroll's leftover skipped columns are gone once the
        // cursor has moved (nvim's `curs_columns` runs before the top line is checked).
        pos.skipcol = sc.frame_skipcol(&pos);
    }
    let skipped = crate::scroll::TopSkipped {
        lines: &lines,
        top: pos.top,
        rows: sc.skipped_rows(pos.skipcol),
    };
    let kept = w.scroll_valid
        == Some(ScrollValid {
            window: w.index,
            cursor,
            top: pos.top,
            revision: p.doc.revision(),
        });
    let sj = viewport::scrolljump_rows(ui_opts.display.scrolljump, height);
    let sms = (win.smoothscroll && win.wrap).then_some(win.width);
    let (top, sms_skip) = if kept {
        (pos.top, None)
    } else {
        let (top, skip) =
            viewport::update_topline_sms(&skipped, pos.top, cline, height, so, sj, sms);
        if top == pos.top || skip.is_some_and(|s| s > 0) {
            (top, skip)
        } else {
            // nvim validates the top line again before it draws: a far jump centred
            // without 'scrolloff' (`scroll_cursor_halfway`) then gets its context rows.
            (
                viewport::update_topline(&lines, top, cline, height, so, sj),
                skip,
            )
        }
    };
    if top != pos.top || sms_skip.is_some() {
        pos.top = top;
        pos.skipcol = sms_skip.unwrap_or(0);
    }
    // A cursor line taller than the window shows the rows around the cursor (nvim's
    // `curs_columns`); 'smoothscroll' keeps what the scroll commands left.
    pos.skipcol = sc.frame_skipcol(&pos);
    let skip = sc.skipped_rows(pos.skipcol);
    let skipcol = pos.skipcol;
    let leftcol = if win.wrap {
        0
    } else {
        crate::ui::window_view::follow_leftcol(&win, start, end, vcol)
    };
    win.skip = skip;
    win.leftcol = leftcol;
    win.cursor = Some((cursor, vcol));
    (win, top, leftcol, skipcol)
}

impl Session {
    /// Panic unless the focused buffer's folds are well formed over it (in range; a manual set flat, an
    /// indent set nested) and every manual fold anchor is live in its document — the fold part of the
    /// invariant walker (#906). Only the focused buffer's folds are reconciled each frame, so only they are
    /// checked against their text.
    pub(crate) fn assert_fold_invariants(&self) {
        let buf = self.ws.focused_buffer();
        if let Some(fv) = self.folds.get(&buf) {
            let lines = crate::app::dispatch::line_count(self.ws.focused().doc.bytes());
            let manual = self.fold_method.get(&buf) != Some(&crate::input::FoldMethod::Indent);
            crate::folds::assert_invariants(fv, lines, manual);
        }
        for (id, a) in &self.fold_anchors {
            if let Some(doc) = self.ws.document(*id) {
                for anchor in a.anchor_ids() {
                    assert!(
                        doc.resolve_anchor(anchor).is_some(),
                        "a fold anchor of buffer {id:?} is not live"
                    );
                }
            }
        }
    }

    /// The per-frame work that precedes the render and the key read: snapshot the focused buffer, refresh
    /// the line index, reconcile folds, publish the crash-rescue state, follow a `:s///c` match, and scroll
    /// each pane to its cursor. `cols`×`term_rows` is the terminal size.
    pub(crate) fn pre_frame(&mut self, cols: u16, term_rows: u16) -> FrameCtx {
        // Snapshot the FOCUSED buffer once for the frame (line index, folds, highlight parse). `text_arc()`
        // is an Arc refcount bump (not an O(buffer) copy): a committed edit builds a fresh Arc, so this handle
        // keeps pointing at the bytes captured here even after the later `&mut ws` borrows edit the doc.
        let (revision, snapshot) = {
            let f = self.ws.focused();
            (f.doc.revision(), f.doc.text_arc())
        };
        // Refresh the line index (rebuilds only on a revision change) so the per-frame row/viewport
        // lookups below are O(log n), not an O(buffer) newline scan. MVP splits share this one buffer.
        self.line_idx
            .refresh_for(self.ws.focused_buffer(), revision, &snapshot);
        self.reconcile_folds(revision, &snapshot);
        self.publish_rescue();
        self.follow_confirm();
        let (text_rows, rects, wins) = self.scroll_windows(cols, term_rows);
        FrameCtx {
            revision,
            snapshot,
            rects,
            wins,
            cols,
            text_rows,
        }
    }

    /// The per-frame work after the render: apply the LSP coordinator's deferred results (opening a buffer
    /// mutates `highlighters`, which the frame's highlight spans borrowed until render ended), then open the
    /// references / code-action pickers it collected.
    pub(crate) fn post_render(&mut self, frame: &FrameCtx) {
        // An LSP edit batch (format, rename, a server's `workspace/applyEdit`) is a risky operation: the
        // typing before it reaches the journal first (RFC-0023 §4, #1202).
        if self.lsp.has_edit_batch() {
            self.sync_journals();
        }
        let recorded = &mut self.recorded;
        self.lsp.apply_pending(
            &mut self.ws,
            &mut self.files,
            &mut self.highlighters,
            &frame.snapshot,
            &mut self.status,
            &mut |ws, ex| run_ex(ex, ws, recorded),
        );
        if let Some(locs) = self.lsp.take_refs() {
            self.overlay = Some(Overlay::Refs(ref_picker::open(locs, self.lsp.cwd())));
        }
        if let Some(actions) = self.lsp.take_actions() {
            self.overlay = Some(Overlay::Actions(action_picker::open(actions)));
        }
    }

    /// Async output (PTY, F-011; LSP diagnostics, F-014) must render without a keypress, so the loop polls
    /// with a timeout while either is live. With neither it keeps the pure blocking read (no spin) — a
    /// terminal ticks fast, an LSP-only session more slowly.
    pub(crate) fn poll_ms(&self) -> Option<u64> {
        let mut poll_ms: Option<u64> = None;
        #[cfg(unix)]
        if !self.terminals.is_empty() {
            poll_ms = Some(TERM_TICK_MS);
        }
        if poll_ms.is_none() && self.lsp.has_live_client() {
            poll_ms = Some(LSP_TICK_MS);
        }
        // Wake for a journal sync that falls due while no key arrives (RFC-0023 §4's idle trigger), and
        // for the idle look at the files on disk (§8).
        for ms in [
            self.journal_wake_ms(),
            self.disk_wake_ms(),
            self.auto_save_wake_ms(),
        ]
        .into_iter()
        .flatten()
        {
            poll_ms = Some(poll_ms.map_or(ms, |p| p.min(ms)));
        }
        poll_ms
    }

    /// The status-line flags after the focused buffer's `[+]` (RFC-0023 §8): `[RO]` for a `'readonly'` buffer (nvim's `%r`, #1211), `[journal!]` when the last
    /// write or sync of its modified buffer's journal failed (#938).
    pub(crate) fn status_flags(&self) -> String {
        let mut flags = String::new();
        let id = self.ws.focused_buffer();
        if self.readonly.get(&self.files, id) {
            flags.push_str("[RO]");
        }
        if self.journal_failed.contains(&id) && self.ws.focused().doc.is_modified() {
            flags.push_str("[journal!]");
        }
        if self.disk_changed.contains(&id) {
            flags.push_str("[disk changed]");
        }
        if self.files.get(&id).is_some_and(|bf| bf.large) {
            flags.push_str("[large]");
        }
        if self.config.safe_mode.is_some() {
            flags.push_str("[Config: safe-mode]");
        }
        flags
    }
}

/// F-011: when a terminal is live the event loop polls for a key with this timeout (≈30fps) instead of
/// blocking, so asynchronous PTY output renders within one tick. Only the terminal-active path polls.
#[cfg(unix)]
const TERM_TICK_MS: u64 = 33;

/// F-014: with only a language server live (no terminal), poll more slowly — diagnostics arrive infrequently,
/// so a ~10fps tick keeps them prompt without spinning.
const LSP_TICK_MS: u64 = 100;
