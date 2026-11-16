//! The session's own Normal (and Visual) chords at a command boundary (#1486): one predicate per chord
//! family, tried in a fixed order by [`Session::normal_chord_owns`]. The order decides which owner wins a
//! key, so it is part of the behaviour: `[z`/`]z`, the operator cancel, `C-w`, the LSP keys, the pickers,
//! page scroll, `z`, `&`, `C-d`/`C-u`, `C-e`/`C-y`.

use crossterm::event::{KeyCode, KeyEvent};

use super::KeyCtx;
use crate::app::dispatch::{is_ctrl, place_cursor};
use crate::app::session::commands::repeat_substitute_lines;
// The pickers come through the session's own imports; this module is their only user.
use crate::app::session::{
    buffer_picker, file_picker, focused_context, line_picker, page_key, palette, Editing, FrameCtx,
    Prefix, ScrollKey, Session,
};
use crate::input::Pending;
use crate::ui::overlay::Overlay;

/// What the chord families read of `key` and the focused view, computed once per key after `[z`/`]z`.
#[derive(Clone, Copy)]
struct ChordKeys {
    /// `z` with no modifier.
    plain_z: bool,
    /// The Native profile: `C-f`/`C-b` are its file / buffer pickers, not page scroll.
    native: bool,
    /// The page-scroll direction `key` names ([`page_key`]), if any.
    page: Option<bool>,
    /// Visual mode at a command boundary: the selection keeps the scroll keys and `z`.
    selection_base: bool,
}

impl Session {
    /// The session's own Normal (and Visual) keys at a command boundary: `[z`/`]z`, `C-w`, `K`, the pickers,
    /// the scroll chords, `z` and `&`. Whether one took `key`. The families are tried in order and the first
    /// that owns `key` wins.
    pub(in crate::app::session) fn normal_chord_owns(
        &mut self,
        key: KeyEvent,
        at: KeyCtx,
        frame: &FrameCtx,
        focused_h: usize,
    ) -> bool {
        if self.bracket_fold_owns(key, at) {
            return true;
        }
        let plain_z = key.code == KeyCode::Char('z') && key.modifiers.is_empty();
        // The Native profile keeps `C-f`/`C-b` for its file / buffer pickers; under Vim they page-scroll.
        let native = self.profile_name == "native";
        let keys = ChordKeys {
            plain_z,
            native,
            page: page_key(key, native),
            selection_base: at.visual && at.pending == Pending::Base,
        };
        self.cancel_op_owns(key, at, keys)
            || self.window_chord_owns(key, at)
            || self.lsp_chord_owns(key, at, frame)
            || self.picker_chord_owns(key, at, keys)
            || self.page_scroll_owns(at, keys, frame, focused_h)
            || self.z_arm_owns(at, keys)
            || self.repeat_substitute_owns(key, at)
            || self.half_scroll_owns(key, at, keys, frame, focused_h)
            || self.line_scroll_owns(key, at, frame, focused_h)
    }

    /// `[z` / `]z` — the engine holds `[`/`]` as a `BracketPrefix`; only the fold-nav second key `z` is a
    /// frontend view move, everything else (`]p`, `[M`, …) stays the engine's. `[z` → the current open
    /// fold's START, `]z` → its END, or the fold around it when the cursor is already there
    /// ([`crate::folds::open_fold_edge`]). No open fold at the cursor → no-op (Vim beeps). Count is ignored
    /// (Vim: `[z`/`]z` take none), so consume it via `take_count`, which also resets the engine's
    /// `BracketPrefix`.
    fn bracket_fold_owns(&mut self, key: KeyEvent, at: KeyCtx) -> bool {
        let Pending::Bracket { open } = at.pending else {
            return false;
        };
        if !(at.normal && key.code == KeyCode::Char('z') && key.modifiers.is_empty()) {
            return false;
        }
        let Session {
            ed: Editing { ws, recorded, .. },
            line_idx,
            engine,
            folds,
            ..
        } = self;
        engine.take_count();
        let buf = ws.focused_buffer();
        let cur_line = line_idx.line_of(ws.focused().view.cursor().0);
        if let Some(fv) = folds.get(&buf) {
            if let Some(target) = crate::folds::open_fold_edge(fv, cur_line, open) {
                place_cursor(ws, recorded, line_idx.nth_line_start(target));
            }
        }
        true
    }

    /// A session chord typed under an armed operator cancels it (Vim's `checkclearop`) and drops its count;
    /// after `z` or `C-w` the key they would have taken next is swallowed.
    fn cancel_op_owns(&mut self, key: KeyEvent, at: KeyCtx, keys: ChordKeys) -> bool {
        let session_chord = keys.plain_z
            || keys.page.is_some()
            || (key.code == KeyCode::Char('&') && key.modifiers.is_empty())
            || ['w', 'p', 'l', 'b', 'f', 'd', 'u', 'e', 'y']
                .iter()
                .any(|&c| is_ctrl(key, c));
        if !(at.cancels_op && session_chord) {
            return false;
        }
        self.engine.take_count();
        if keys.plain_z || is_ctrl(key, 'w') {
            self.prefix = Prefix::Swallow;
        }
        true
    }

    /// `C-w` arms the window prefix with the count typed before it.
    fn window_chord_owns(&mut self, key: KeyEvent, at: KeyCtx) -> bool {
        if !(at.chord && is_ctrl(key, 'w')) {
            return false;
        }
        self.prefix = Prefix::Window(self.engine.take_count());
        true
    }

    /// F-014: `K` = hover, `<C-]>` = goto-definition (when the focused buffer has a live server; else both
    /// fall through, unbound in the grammar). The coordinator sends the request.
    fn lsp_chord_owns(&mut self, key: KeyEvent, at: KeyCtx, frame: &FrameCtx) -> bool {
        let Session {
            ed: Editing { ws, files, .. },
            engine,
            lsp,
            ..
        } = self;
        if !lsp.on_normal_key(key, at.chord, ws, files, &frame.snapshot) {
            return false;
        }
        engine.take_count();
        true
    }

    /// The picker chords: `C-p` the command palette, `C-l` the buffer-line picker and, under the Native
    /// profile, `C-b` the buffer picker and `C-f` the file finder.
    fn picker_chord_owns(&mut self, key: KeyEvent, at: KeyCtx, keys: ChordKeys) -> bool {
        if !at.chord {
            return false;
        }
        let Session {
            ed: Editing { ws, .. },
            engine,
            overlay,
            ..
        } = self;
        // `C-p` opens the command palette (F-004 #2), context-filtered to the focused view.
        if is_ctrl(key, 'p') {
            engine.take_count();
            *overlay = Some(Overlay::Palette(palette::open(
                &focused_context(ws),
                engine,
            )));
            return true;
        }
        // `C-l` opens the buffer-line fuzzy picker (F-013 NAT-3). Normal-only, so the Emacs profile's
        // non-modal C-l (recenter) is unaffected; C-l is unbound in the Vim/Native Normal grammar.
        if is_ctrl(key, 'l') {
            engine.take_count();
            *overlay = Some(Overlay::Lines(line_picker::open(ws.focused().doc.bytes())));
            return true;
        }
        // Native profile: `C-b` opens the buffer picker and `C-f` the fuzzy file finder (F-013 NAT-3). Normal
        // only, so the Emacs profile's non-modal C-b / C-f (backward-/forward-char) is unaffected. Under the
        // Vim profile they are Vim's page scroll ([`Self::page_scroll_owns`]).
        if keys.native && is_ctrl(key, 'b') {
            engine.take_count();
            *overlay = Some(Overlay::Buffers(buffer_picker::open(ws)));
            return true;
        }
        if keys.native && is_ctrl(key, 'f') {
            engine.take_count();
            *overlay = Some(Overlay::Files(file_picker::open()));
            return true;
        }
        false
    }

    /// Page scroll (`C-f`/`C-b`, `<PageDown>`/`<PageUp>`, `<S-Down>`/`<S-Up>`, `<S-CR>`), in Normal and
    /// Visual and, through the Insert arm, in Insert. `{count}` scrolls that many pages.
    fn page_scroll_owns(
        &mut self,
        at: KeyCtx,
        keys: ChordKeys,
        frame: &FrameCtx,
        focused_h: usize,
    ) -> bool {
        let (Some(forward), true) = (keys.page, at.chord || keys.selection_base) else {
            return false;
        };
        let count = self.engine.take_count().max(1) as usize;
        self.scroll_key(ScrollKey::Page { forward, count }, frame, focused_h);
        true
    }

    /// `z` arms the scroll/fold prefix with the count typed before it. In Normal (scroll + fold verbs) and
    /// Visual (so `zf` folds the selection); plain `z` (no ctrl), never on the command line.
    fn z_arm_owns(&mut self, at: KeyCtx, keys: ChordKeys) -> bool {
        if !((at.chord || keys.selection_base) && keys.plain_z) {
            return false;
        }
        self.prefix = Prefix::Z(self.engine.take_count());
        true
    }

    /// `&` repeats the last `:s` on the current line, keeping its flags (nvim's `&-default` mapping). The
    /// session owns the key so it can read the count; see [`repeat_substitute_lines`].
    fn repeat_substitute_owns(&mut self, key: KeyEvent, at: KeyCtx) -> bool {
        if !(at.chord && key.code == KeyCode::Char('&') && key.modifiers.is_empty()) {
            return false;
        }
        let report = self.ui_opts.report;
        let Session {
            ed:
                Editing {
                    ws,
                    recorded,
                    status,
                    ..
                },
            engine,
            last_substitute,
            ..
        } = self;
        let count = engine.take_count().max(1) as usize;
        status.give(repeat_substitute_lines(
            ws,
            recorded,
            last_substitute.as_ref(),
            count,
            report,
        ));
        true
    }

    /// `C-d` / `C-u` scroll the view and move the cursor by the window's 'scroll' lines (default half the
    /// pane). A count sets 'scroll' for the window first (capped at the pane height), as in Vim.
    fn half_scroll_owns(
        &mut self,
        key: KeyEvent,
        at: KeyCtx,
        keys: ChordKeys,
        frame: &FrameCtx,
        focused_h: usize,
    ) -> bool {
        if !((at.chord || keys.selection_base) && (is_ctrl(key, 'd') || is_ctrl(key, 'u'))) {
            return false;
        }
        let count = self.engine.take_count() as usize;
        let ws = &mut self.ed.ws;
        if count > 0 {
            ws.set_scroll(ws.focus(), count.min(focused_h.max(1)), focused_h);
        }
        let amount = ws.focused().view.scroll(focused_h);
        self.scroll_key(
            ScrollKey::Half {
                forward: is_ctrl(key, 'd'),
                amount,
            },
            frame,
            focused_h,
        );
        true
    }

    /// `C-e` / `C-y` scroll the view one line down / up, nudging the cursor only if it would leave the
    /// scrolloff band. Frontend-only (viewport concern); `{count}` scrolls that many lines.
    fn line_scroll_owns(
        &mut self,
        key: KeyEvent,
        at: KeyCtx,
        frame: &FrameCtx,
        focused_h: usize,
    ) -> bool {
        if !(at.chord && (is_ctrl(key, 'e') || is_ctrl(key, 'y'))) {
            return false;
        }
        let count = self.engine.take_count().max(1) as usize;
        self.scroll_key(
            ScrollKey::Lines {
                up: is_ctrl(key, 'e'),
                count,
            },
            frame,
            focused_h,
        );
        true
    }
}
