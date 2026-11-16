//! The session's input entry points (#878, #1496): [`Session::on_key`] and [`Session::on_paste`], the
//! answer to the prompt the last frame showed, and the routing of one key through its owners in the
//! order they claim it. Split from `session.rs`.

use crossterm::event::{KeyEvent, KeyEventKind};
use ruse_core::{BytePos, ExCmd, GroupKind, Mode};

use super::{scrolloff_for, Editing, Flow, FrameCtx, Prefix, Session};
use crate::app::dispatch::{note_ex, run_repeat};
use crate::input::Feed;

impl Session {
    /// The key typed at the prompt the last frame showed (true: the key is used up). At the hit-enter
    /// prompt (#1222) it clears the message: `<CR>`, `<NL>`, `<Space>` and `CTRL-C` only do that; any other
    /// key, `<Esc>` too, then runs as typed, `:` opening the command line, as nvim v0.12.6's `wait_return`
    /// puts it back into the typeahead. At `-- More --` (#1235) the paging keys move the message and `q`
    /// ends it ([`crate::ui::message::Pager::more_key`]).
    pub(super) fn answer_prompt(&mut self, key: KeyEvent) -> bool {
        use crate::ui::message::{Answer, Prompt};
        let answer = match std::mem::take(&mut self.pager.prompt) {
            Prompt::None => return false,
            Prompt::HitEnter => self.pager.hit_enter_key(key),
            Prompt::More => self.pager.more_key(key),
        };
        match answer {
            Answer::Stay => true,
            Answer::Done => {
                self.ed.status.clear();
                true
            }
            Answer::DoneThenRun => {
                self.ed.status.clear();
                false
            }
        }
    }

    /// Route one key through the session (#878): the macro state machine, the LSP hover / completion
    /// intercepts, a `:s///c` confirm loop, an open picker, a terminal buffer, the session prefixes and
    /// chords, `H`/`M`/`L`, and finally the input engine and its resolution. `frame` is the frame built
    /// for this key by [`Session::pre_frame`].
    pub(crate) fn on_key(
        &mut self,
        raw_key: KeyEvent,
        from_replay: bool,
        frame: &FrameCtx,
    ) -> Flow {
        let status_before = self.ed.status.serial();
        self.last_input = std::time::Instant::now();
        self.auto_saved_since_input = false;
        if !from_replay {
            // A typed key comes after any replay drained; a block left open would join this key's change.
            self.end_macro_undo();
            self.sync_undo_before_typed();
        }
        let ro_watch = self.readonly.watch(&self.ed.ws, &self.ed.files);
        let trace_before = (self.ed.recorded.len(), self.ed.ws.focused_buffer());
        // A replayed key's edits are the macro's (INV-ORIGIN, #880).
        self.key_typed = !from_replay;
        self.route_as(raw_key, from_replay, frame);
        self.key_typed = false;
        self.readonly
            .warn(ro_watch, &self.ed.ws, &mut self.ed.status);
        self.trace_watch
            .after_input(trace_before, &mut self.ed.ws, &mut self.ed.recorded);
        if !from_replay && !self.emacs_profile {
            // A typed `@{reg}` (or `@:`, `Q`) queued its replay: one undo step for all of it (#939).
            if self.macros.replay_pending() && !self.ed.ws.focused().doc.in_undo_block() {
                self.ed.ws.focused_doc_mut().begin_undo_block();
                self.macro_undo = Some(self.ed.ws.focused_buffer());
                note_ex(ExCmd::Begin(GroupKind::Macro), &mut self.ed.recorded);
            }
        }
        if let Some(key) = self.swap_answer.take() {
            if let Some(prompt) = self.swap.take() {
                self.answer_swap(prompt, key, frame);
            } else if let Some(prompt) = self.changed_prompt.take() {
                self.answer_changed(prompt, key, frame);
            } else if let Some(prompt) = self.emacs_prompt.take() {
                self.answer_emacs(prompt, key);
            }
        }

        if self.failed_since(status_before) {
            self.macros.flush_replay();
        }
        self.settle_interrupt();
        if self.ed.quit {
            Flow::Quit
        } else {
            Flow::Continue
        }
    }

    /// Route one bracketed paste (#600, `:help paste`). The text never becomes keys, so no mapping, macro
    /// prefix, session prefix or chord sees it. Line endings are normalised first (CRLF and CR become LF,
    /// as nvim's TUI pastes). A terminal buffer forwards it to its child (bracketed if the child asked);
    /// a picker or a `:s///c` confirm loop declines it with a message; everything else goes through
    /// [`InputEngine::paste`]. A paste does not end a macro recording and is not recorded into it.
    pub(crate) fn on_paste(&mut self, raw: &str, frame: &FrameCtx) -> Flow {
        self.last_input = std::time::Instant::now();
        self.auto_saved_since_input = false;
        let text = crate::input::paste_text(raw);
        if text.is_empty() {
            return Flow::Continue;
        }
        self.lsp.clear_hover();
        self.prefix = Prefix::None;
        self.meta_tail = None;
        if self.overlay_open() {
            self.ed.status.warn("paste ignored: close the picker first");
            return Flow::Continue;
        }
        let mode = self.ed.ws.focused().view.mode();
        #[cfg(unix)]
        if matches!(mode, Mode::Terminal | Mode::TerminalNormal) {
            if let Some(t) = self.terminals.get_mut(&self.ed.ws.focused_buffer()) {
                t.paste(&text);
                return Flow::Continue;
            }
        }
        self.sync_undo_before_typed();
        let visual_append = self.visual_paste_appends(mode, &frame.snapshot);
        let out = self.engine.paste(&text, mode, visual_append);
        let ro_watch = self.readonly.watch(&self.ed.ws, &self.ed.files);
        let report = self.ui_opts.report;
        let trace_before = (self.ed.recorded.len(), self.ed.ws.focused_buffer());
        let Session {
            ed:
                Editing {
                    ws,
                    files,
                    recorded,
                    status,
                    quit,
                },
            ..
        } = self;
        match out {
            Feed::Cmd(cmd) => self.ed.run(cmd),
            Feed::Replay(cmds) => {
                for cmd in cmds {
                    self.ed.run(cmd);
                }
            }
            Feed::Repeat(r) => run_repeat(r, ws, files, recorded, status, quit, report),
            _ => {}
        }
        self.readonly
            .warn(ro_watch, &self.ed.ws, &mut self.ed.status);
        self.trace_watch
            .after_input(trace_before, &mut self.ed.ws, &mut self.ed.recorded);
        self.settle_interrupt();
        if self.ed.quit {
            Flow::Quit
        } else {
            Flow::Continue
        }
    }

    /// For a paste over a blockwise Visual/Select selection: whether deleting the block leaves the caret
    /// past the end of its top row, so the text must be appended (`a`) rather than inserted (`i`) to land
    /// where the block began, as nvim does. Charwise and linewise pastes replace the selection in the core.
    pub(super) fn visual_paste_appends(&self, mode: Mode, bytes: &[u8]) -> bool {
        let (Mode::Visual { kind } | Mode::Select { kind }) = mode else {
            return false;
        };
        if kind != ruse_core::SelectKind::Blockwise {
            return false;
        }
        let span = self
            .ed
            .ws
            .focused()
            .view
            .block_spans(bytes)
            .and_then(|rows| rows.first().copied());
        let Some((BytePos(s), BytePos(e))) = span else {
            return false;
        };
        let at_line_start = s == 0 || bytes.get(s - 1) == Some(&b'\n');
        let rest_empty = matches!(bytes.get(e), None | Some(b'\n'));
        !at_line_start && rest_empty
    }

    /// Whether a modal overlay owns the keystream: a `:s///c` confirm loop or an open picker. The macro
    /// layer must not take its `q`/`@` (#851).
    pub(super) fn overlay_open(&self) -> bool {
        self.confirm.is_some()
            || self.swap.is_some()
            || self.changed_prompt.is_some()
            || self.emacs_prompt.is_some()
            || self.overlay.is_some()
    }

    /// The focused window's `'scrolloff'` as the rows of context the viewport keeps.
    pub(super) fn scrolloff_rows(&self) -> usize {
        scrolloff_for(self.ed.ws.focused().view, self.scrolloff)
    }

    pub(super) fn route(&mut self, raw_key: KeyEvent, from_replay: bool, frame: &FrameCtx) {
        // Emacs View mode (`view-read-only`) takes its keys between commands (#619).
        if self.emacs_profile
            && !self.overlay_open()
            && self.engine.emacs_idle()
            && self.view_key(raw_key, frame)
        {
            return;
        }
        self.route_key(raw_key, from_replay, frame);
        // A read-only buffer refuses a change in Emacs's words (#619).
        if self.emacs_profile
            && self.ed.status.is_error()
            && self.ed.status.text() == ruse_core::E21
        {
            let name = self
                .ed
                .ws
                .buffer_name(self.ed.ws.focused_buffer())
                .unwrap_or("*scratch*");
            let base = std::path::Path::new(name)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            self.ed
                .status
                .error(format!("Buffer is read-only: #<buffer {base}>"));
        }
        // A window key that stands for an Ex command (`CTRL-W q` = `:quit`) runs it now, before the next
        // key, through the Ex dispatch and its unsaved-changes gate (#619).
        if let Some(line) = self.window_ex.take() {
            self.ed.status.clear();
            self.run_ex_pieces(vec![line.to_string()], frame);
        }
    }

    pub(super) fn route_key(&mut self, raw_key: KeyEvent, from_replay: bool, frame: &FrameCtx) {
        if raw_key.kind == KeyEventKind::Release {
            return;
        }
        // The owners, in the order they claim a key; the first that takes it ends the routing.
        let Some(key) = self.macro_key(raw_key, from_replay) else {
            return;
        };
        // F-014: any key dismisses a shown hover panel (a fresh `K` re-populates it after its response).
        self.lsp.clear_hover();
        if self.popup_owns(key, frame) || self.prompt_owns(key) || self.picker_owns(key, frame) {
            return;
        }
        #[cfg(unix)]
        if self.terminal_owns(key) {
            return;
        }
        // The focused pane's visible height (for the scroll / recenter commands).
        let focused_h = self.focused_height(frame);
        if self.zf_owns(key)
            || self.z_prefix_owns(key, frame, focused_h)
            || self.window_prefix_owns(key)
        {
            return;
        }
        let at = self.key_ctx();
        if self.normal_chord_owns(key, at, frame, focused_h)
            || self.insert_chord_owns(key, frame, focused_h)
            || self.screen_line_owns(key, at, frame, focused_h)
        {
            return;
        }
        self.feed_engine(key, frame);
    }

    /// The focused pane's visible height in rows.
    pub(super) fn focused_height(&self, frame: &FrameCtx) -> usize {
        frame
            .rects
            .get(self.ed.ws.focus())
            .map_or(frame.text_rows as usize, |r| r.h as usize)
    }
}
