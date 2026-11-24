//! The session's own Normal, Visual and Insert keys, offered a key before the input engine (#1354): the
//! `z`, `zf` and `C-w` prefixes, the chords at a command boundary, the Insert keys the engine cannot
//! resolve without the buffer, and `H`/`M`/`L`. Each borrows the `Session` fields it uses.

use super::{
    insert_page_key, is_ctrl, place_cursor, run_scroll, viewport, viewport_line, Command,
    DocumentId, Editing, Feed, FrameCtx, HScroll, HashMap, KeyCode, KeyEvent, KeyModifiers, Mode,
    PaneLines, PaneText, Pending, Prefix, ScrollKey, Session, SplitDir, Workspace,
};
use crate::input::ScreenMotion;

mod normal_chord;
mod z_fold;

/// The focused view's state the Normal intercepts read, taken once per key.
#[derive(Clone, Copy)]
pub(super) struct KeyCtx {
    /// Vim Normal mode, no command line or command-line window open.
    pub(super) normal: bool,
    /// Visual mode (not Select).
    pub(super) visual: bool,
    /// What the engine waits for.
    pub(super) pending: Pending,
    /// Normal at a command boundary: the session's own keys fire.
    pub(super) chord: bool,
    /// Normal under an armed operator: a session key cancels it.
    pub(super) cancels_op: bool,
}

/// The focused buffer's folds.
pub(super) fn focused_folds<'a>(
    folds: &'a HashMap<DocumentId, Vec<crate::folds::Fold>>,
    ws: &Workspace,
) -> &'a [crate::folds::Fold] {
    crate::folds::for_buffer(folds, ws.focused_buffer())
}

impl Session {
    /// `zf{motion}`: while the `zf` prefix is armed, the keys after it. Whether it took `key`.
    pub(super) fn zf_owns(&mut self, key: KeyEvent) -> bool {
        let Session {
            ed:
                Editing {
                    ws,
                    recorded,
                    status,
                    ..
                },
            line_idx,
            prefix,
            folds,
            ..
        } = self;
        // `zf{motion}` (Normal mode): keep buffering keys after `zf` until they resolve to a linewise
        // motion, then create a CLOSED fold over the line range the motion covers — sourced from the same
        // `reindent_range` seam `=` uses, so `zf` folds exactly the lines `=`/`d` would touch. The cursor
        // lands on the fold's first line (matches nvim). This is folds "slice 2" — the Normal-mode
        // counterpart of the Visual `zf` below.
        if let Prefix::Zf(pending) = prefix {
            let mut buf = std::mem::take(pending);
            *prefix = Prefix::None;
            // A char key extends the motion; Esc or any non-char key abandons the pending `zf`.
            let ch = match key.code {
                KeyCode::Char(c)
                    if (key.modifiers - crossterm::event::KeyModifiers::SHIFT).is_empty() =>
                {
                    Some(c)
                }
                _ => None,
            };
            let Some(c) = ch else {
                return true; // cancelled — the `zf` prefix is already cleared
            };
            buf.push(c);
            match crate::folds::parse_fold_motion(&buf) {
                crate::folds::FoldMotionParse::More => {
                    *prefix = Prefix::Zf(buf); // still accumulating (a count or a multi-key motion)
                }
                crate::folds::FoldMotionParse::Cancel => {} // not a fold motion — drop it
                crate::folds::FoldMotionParse::Ready { motion, count } => {
                    // A one-line fold is a fold too (nvim): it never shows closed ('foldminlines').
                    if let Some((start, end)) = ws.reindent_range(motion, count) {
                        let fv = folds.entry(ws.focused_buffer()).or_default();
                        crate::folds::insert_closed_fold(fv, start, end);
                        place_cursor(ws, recorded, line_idx.nth_line_start(start));
                        status.echo(format!("folded {} lines", end - start + 1));
                    }
                }
            }
            return true;
        }
        false
    }

    /// The `z` prefix (`Prefix::Z`), or a key swallowed after a cancelled operator: whether it took `key`.
    ///
    /// `z` scroll prefix: the second key recenters the view on the cursor line (`zz`/`z.` center,
    /// `zt`/`z<CR>` top, `zb`/`z-` bottom). Only `top` moves; the per-frame scroll pass then applies
    /// `scrolloff`, so `zt`/`zb` land the line `scrolloff` rows inside the edge, as in Vim.
    pub(super) fn z_prefix_owns(
        &mut self,
        key: KeyEvent,
        frame: &FrameCtx,
        focused_h: usize,
    ) -> bool {
        match self.prefix {
            Prefix::Swallow => self.prefix = Prefix::None,
            Prefix::Z(count) => {
                self.prefix = Prefix::None;
                if !(self.z_recenter(key, count, frame, focused_h)
                    || self.z_hscroll(key, count, frame)
                    || self.z_paste_yank(key, count))
                {
                    self.z_fold(key, count, frame);
                }
            }
            _ => return false,
        }
        true
    }

    /// `z` + a scroll-align verb (`zz`/`zt`/`zb`/`z.`/`z-`/`z<CR>`/`z+`/`z^`): whether `key` was one.
    fn z_recenter(
        &mut self,
        key: KeyEvent,
        count: u32,
        frame: &FrameCtx,
        focused_h: usize,
    ) -> bool {
        let so = self.scrolloff_rows();
        let Session {
            ed,
            line_idx,
            folds,
            ..
        } = self;
        let snapshot = &frame.snapshot;
        // Scroll-align verbs (zz/zt/zb/z./z-/z<CR>/z+/z^). `{count}` first moves the cursor to line
        // `count` (clamped to the last line): `zz`/`zt`/`zb` keep its column, the others put it on the
        // first non-blank, with or without a count (`:help z<CR>`). Neither is a jump.
        let to = match key.code {
            KeyCode::Char('z' | '.') => Some(viewport::RecenterTo::Center),
            KeyCode::Char('t' | '+') | KeyCode::Enter => Some(viewport::RecenterTo::Top),
            KeyCode::Char('b' | '-' | '^') => Some(viewport::RecenterTo::Bottom),
            _ => None,
        };
        if let Some(to) = to {
            let cur = line_idx.line_of(ed.ws.focused().view.cursor().0);
            let last = line_idx.line_of(snapshot.len().saturating_sub(1));
            let top = ed.ws.focused().view.top();
            let row = match (key.code, count) {
                // `z+`: the line just below the window goes to the top (nvim's `nv_zet`).
                (KeyCode::Char('+'), 0) => (top + focused_h).min(last),
                // `z^`: the line just above the window goes to the bottom.
                (KeyCode::Char('^'), 0) => top.saturating_sub(1),
                // `{count}z^`: put line `count` at the bottom (keeping 'scrolloff'), then the line now at
                // the top goes to the bottom.
                (KeyCode::Char('^'), n) => {
                    let so = so.min(focused_h.saturating_sub(1) / 2);
                    (n as usize - 1)
                        .min(last)
                        .saturating_sub(focused_h.saturating_sub(1 + so))
                }
                (_, 0) => cur,
                (_, n) => (n as usize - 1).min(last),
            };
            if matches!(key.code, KeyCode::Char('z' | 't' | 'b')) {
                if row != cur {
                    let (n, m) = if row > cur {
                        (row - cur, ruse_core::Motion::Down)
                    } else {
                        (cur - row, ruse_core::Motion::Up)
                    };
                    ed.run(Command::Move(n as u32, m));
                }
            } else {
                let start = line_idx.nth_line_start(row);
                let blank = snapshot[start.min(snapshot.len())..]
                    .iter()
                    .take_while(|b| matches!(b, b' ' | b'\t'))
                    .count();
                // An all-blank line keeps the cursor on its last blank, not on the newline.
                let at_eol = matches!(snapshot.get(start + blank), Some(b'\n') | None);
                let off = if at_eol {
                    blank.saturating_sub(1)
                } else {
                    blank
                };
                place_cursor(&mut ed.ws, &mut ed.recorded, start + off);
            }
            // Over closed folds the rows are counted as nvim counts them (a fold is one row).
            let fv = focused_folds(folds, &ed.ws);
            let new_top = match frame.wins.get(ed.ws.focus()) {
                Some(win) if fv.iter().any(|f| f.closed) => {
                    let lines = PaneLines {
                        text: PaneText {
                            bytes: snapshot,
                            idx: line_idx,
                        },
                        win,
                        folds: fv,
                        virt: &[],
                        height: focused_h,
                    };
                    viewport::recenter_lines(&lines, row, focused_h, to)
                }
                _ => viewport::recenter(row, focused_h, to),
            };
            ed.ws.set_top(ed.ws.focus(), new_top);
            return true;
        }
        false
    }

    /// `z` + a horizontal scroll verb (`zh`/`zl`/`zH`/`zL`/`zs`/`ze`): whether `key` was one.
    fn z_hscroll(&mut self, key: KeyEvent, count: u32, frame: &FrameCtx) -> bool {
        let Session {
            ed: Editing { ws, recorded, .. },
            line_idx,
            folds,
            ..
        } = self;
        let snapshot = &frame.snapshot;
        let rects = &frame.rects;
        // Horizontal scroll (#615): `zh` / `zl` (`z<Left>` / `z<Right>`) `count` columns, `zH` / `zL`
        // half the window's width, `zs` / `ze` the cursor's column first / last, then the cursor
        // moves back inside the window if it left it. Nothing while the window wraps (nvim).
        let n = count.max(1) as usize;
        let half = rects.get(ws.focus()).map_or(0, |r| usize::from(r.w) / 2);
        let hcmd = match key.code {
            KeyCode::Char('h') | KeyCode::Left => Some(HScroll::Left(n)),
            KeyCode::Char('l') | KeyCode::Right => Some(HScroll::Right(n)),
            KeyCode::Char('H') => Some(HScroll::Left(n.saturating_mul(half))),
            KeyCode::Char('L') => Some(HScroll::Right(n.saturating_mul(half))),
            KeyCode::Char('s') => Some(HScroll::CursorFirst),
            KeyCode::Char('e') => Some(HScroll::CursorLast),
            _ => None,
        };
        if let Some(hcmd) = hcmd {
            let Some(win) = frame.wins.get(ws.focus()) else {
                return true;
            };
            let (cursor, mode) = (ws.focused().view.cursor().0, ws.focused().view.mode());
            let text = PaneText {
                bytes: snapshot,
                idx: line_idx,
            };
            let line = line_idx.line_of(cursor.min(snapshot.len()));
            let start = text.start(line);
            let lay = win.lay(text.line(line));
            let folded = folds
                .get(&ws.focused_buffer())
                .is_some_and(|fv| crate::folds::closed_range(fv, line).is_some());
            let scrolled = crate::ui::window_view::hscroll(
                win,
                &lay,
                cursor.saturating_sub(start),
                win.tab_end(mode),
                folded,
                hcmd,
            );
            if let Some((leftcol, to)) = scrolled {
                ws.set_leftcol(ws.focus(), leftcol);
                if let Some(b) = to {
                    place_cursor(ws, recorded, start + b);
                }
            }
            return true;
        }
        false
    }

    /// `zp`/`zP`/`zy`: whether `key` was one.
    fn z_paste_yank(&mut self, key: KeyEvent, count: u32) -> bool {
        let Session {
            ed: Editing { ws, .. },
            engine,
            ..
        } = self;
        // `zp` / `zP` (Normal) paste like `p` / `P` without a block's trailing spaces; `zy` yanks like
        // `y` without a block's trailing white space: over the selection in Visual, and in Normal as the
        // `y` operator (its motions are never blockwise, so it is `y` there), with the count kept.
        if (key.modifiers - KeyModifiers::SHIFT).is_empty() {
            let visual = matches!(ws.focused().view.mode(), Mode::Visual { .. });
            match key.code {
                KeyCode::Char(c @ ('p' | 'P')) if !visual => {
                    let cmd = Command::PasteBlockInner {
                        after: c == 'p',
                        count: count.max(1),
                    };
                    self.ed.run(cmd);
                    return true;
                }
                KeyCode::Char('y') if visual => {
                    let cmd = Command::VisualOp(ruse_core::VisualOp::YankTrim);
                    self.ed.run(cmd);
                    return true;
                }
                KeyCode::Char('y') => {
                    let mode = ws.focused().view.mode();
                    let digits = if count > 0 {
                        count.to_string()
                    } else {
                        String::new()
                    };
                    for d in digits.chars().chain(['y']) {
                        engine.feed(KeyEvent::new(KeyCode::Char(d), KeyModifiers::NONE), mode);
                    }
                    return true;
                }
                _ => {}
            }
        }
        false
    }

    /// A `C-w` window prefix: whether it took `key` (a count digit, or the window command).
    pub(super) fn window_prefix_owns(&mut self, key: KeyEvent) -> bool {
        let Session {
            ed: Editing { ws, .. },
            prefix,
            window_ex,
            ..
        } = self;
        // F-007 window layer: a `C-w` prefix (Normal mode, no command-line, not Insert where `C-w`
        // deletes a word) arms the next key as a window command. A thin frontend intercept for MVP;
        // F-003's keymap router will absorb it into a proper layer.
        if let Prefix::Window(count) = *prefix {
            // A count may also follow `C-w` (`C-w 3 w`, as in nvim); a leading `0` is not a count.
            if let KeyCode::Char(d @ '0'..='9') = key.code {
                if key.modifiers.is_empty() && (d != '0' || count > 0) {
                    let digit = d.to_digit(10).unwrap_or(0);
                    *prefix = Prefix::Window(count.saturating_mul(10).saturating_add(digit));
                    return true;
                }
            }
            *prefix = Prefix::None;
            dispatch_window(key, count, ws, window_ex);
            return true;
        }
        false
    }

    /// Where the focused view stands for the Normal intercepts below.
    ///
    /// The command-line window (`q:`/`q/`/`q?`) owns the keystream while open, like the `:` line: suppress
    /// every Normal-mode intercept below so its keys (`j`/`k`/`<CR>`/`<Esc>`/`<C-c>`) reach `engine.feed`
    /// → `feed_cmdwin` untouched.
    ///
    /// The Emacs profile is modeless: its view starts in `Mode::Normal` but none of these Vim Normal
    /// intercepts are Emacs keys, so they never fire there (`C-u 3 x`, `C-f`, `C-e` as the first keys of
    /// an Emacs session reach `feed_emacs`, #1156).
    ///
    /// Visual only: in Select a typed `z` replaces the selection (#792).
    ///
    /// What the engine is waiting for decides who owns this key (#679, #850). The session's own Normal
    /// keys below (`z`, `C-w`, `&`, the scroll chords, the pickers, `K`) fire only at a command boundary;
    /// while the engine waits for an argument (`fz`, `rz`, `mz`, `"z`, `gM`, `]M`, a leader key) the key is
    /// that argument and goes to the engine. None of them is a motion, so under an armed operator they
    /// cancel it and do nothing (Vim's `checkclearop`), and the count typed before them is theirs, never
    /// the next command's.
    pub(super) fn key_ctx(&self) -> KeyCtx {
        let (ws, engine) = (&self.ed.ws, &self.engine);
        let normal = !self.emacs_profile
            && matches!(ws.focused().view.mode(), Mode::Normal)
            && engine.cmdline().is_none()
            && engine.cmdwin().is_none();
        let visual = matches!(ws.focused().view.mode(), Mode::Visual { .. });
        let pending = engine.pending();
        let chord = normal && pending == Pending::Base;
        let cancels_op = normal && pending == Pending::Operator;
        KeyCtx {
            normal,
            visual,
            pending,
            chord,
            cancels_op,
        }
    }

    /// The Insert-mode keys the session resolves for the engine: `CTRL-E`/`CTRL-Y`, the page keys,
    /// `CTRL-N`/`CTRL-P` and `CTRL-X CTRL-L`/`CTRL-N`/`CTRL-P`. Whether one took `key`.
    pub(super) fn insert_chord_owns(
        &mut self,
        key: KeyEvent,
        frame: &FrameCtx,
        focused_h: usize,
    ) -> bool {
        let Session {
            ed,
            emacs_profile,
            engine,
            ..
        } = self;
        // `i_CTRL-E` / `i_CTRL-Y` (Insert mode) — insert the char directly BELOW / ABOVE the caret at the
        // same column. The engine has no buffer, so resolve the adjacent-line char here and hand it to the
        // engine, which emits (and dot-records) a concrete `InsertChar`. `None` (short/absent adjacent
        // line) is a Vim no-op. Guarded by `insert_plain_text_ctx` so a pending Insert prefix (e.g.
        // `CTRL-R`'s register name) still owns the key. Normal-mode `C-e`/`C-y` scroll is handled above.
        // These Insert-mode intercepts are Vim keys too: an Emacs session is in Insert after its first
        // printable key, and there `C-e`, `C-y`, `C-n`, `C-p`, `C-l` and the page keys are Emacs commands
        // (#1156).
        let vim_insert = !*emacs_profile && matches!(ed.ws.focused().view.mode(), Mode::Insert);
        if vim_insert && (is_ctrl(key, 'e') || is_ctrl(key, 'y')) && engine.insert_plain_text_ctx()
        {
            // An active completion cycle owns the key first: `CTRL-E` cancels it, `CTRL-Y` accepts (#805).
            if let Some(feed) = engine.complete_end(is_ctrl(key, 'e')) {
                if let Feed::Cmd(cmd) = feed {
                    ed.run(cmd);
                }
                return true;
            }
            let ch = ed.ws.adjacent_line_char(is_ctrl(key, 'y'));
            if let Feed::Cmd(cmd) = engine.insert_copy_char(ch) {
                ed.run(cmd);
            }
            return true;
        }
        // `i_<PageDown>` / `i_<PageUp>` / `i_<S-Down>` / `i_<S-Up>` — page scroll from Insert, a caret jump
        // that breaks undo and restarts the `.` record like `i_<Down>` (nvim v0.12.6).
        if let (true, Some(forward)) = (vim_insert, insert_page_key(key)) {
            if engine.insert_plain_text_ctx() {
                // The jump breaks undo first (it changes no text, so the scroll below sees the same lines).
                ed.run(engine.insert_page_jump());
                self.scroll_key(ScrollKey::Page { forward, count: 1 }, frame, focused_h);
                return true;
            }
        }
        // `i_CTRL-N` / `i_CTRL-P` (Insert mode) — keyword completion from the CURRENT buffer (F-003). The
        // engine has no buffer, so on the FIRST key the frontend resolves the base + candidate set (nvim
        // scan order) and hands them to the engine, which caches them and cycles the selection; subsequent
        // keys just step the cached cycle. The engine emits (and dot-records) a `CompleteWord` swapping the
        // completion region. Guarded by `insert_plain_text_ctx` (a pending Insert prefix still owns its key).
        // Other completion sources / the popup menu are out of scope for this slice (deferred).
        // `i_CTRL-X CTRL-N` / `CTRL-X CTRL-P` search the current buffer only — the same source this
        // `CTRL-N`/`CTRL-P` already uses — so the submode key starts a fresh keyword cycle (#602).
        let ctrl_x_keyword = engine.insert_ctrl_x_pending();
        if vim_insert
            && (is_ctrl(key, 'n') || is_ctrl(key, 'p'))
            && (ctrl_x_keyword || engine.insert_plain_text_ctx())
        {
            let forward = is_ctrl(key, 'n');
            if ctrl_x_keyword {
                engine.end_insert_ctrl_x();
            }
            let feed = if engine.completion_active() && !ctrl_x_keyword {
                engine.complete_cycle(forward)
            } else {
                let (base, cands) = ed.ws.keyword_completion();
                engine.complete_start(base, cands, forward)
            };
            if let Feed::Cmd(cmd) = feed {
                ed.run(cmd);
            }
            return true;
        }
        // `i_CTRL-X CTRL-L` (Insert mode) — whole-line completion from the CURRENT buffer (F-003). `CTRL-X`
        // armed the submode in the engine (`insert_ctrl_x_pending`); this `CTRL-L` resolves the candidate
        // lines here (the engine has no buffer — the same split as `i_CTRL-N`) and starts the cycle. A bare
        // `CTRL-L` while a cycle is already active steps it FORWARD (nvim's whole-line continuation key);
        // `CTRL-N`/`CTRL-P` cycle it too (the block above). Deferred: the `CTRL-X CTRL-F` (filename) and other
        // sources, and the popup menu — all out of scope for this slice.
        if vim_insert && is_ctrl(key, 'l') {
            if engine.insert_ctrl_x_pending() {
                let (base, cands) = ed.ws.line_completion();
                if let Feed::Cmd(cmd) = engine.complete_line_start(base, cands) {
                    ed.run(cmd);
                }
                return true;
            }
            if engine.completion_active() && engine.insert_plain_text_ctx() {
                if let Feed::Cmd(cmd) = engine.complete_cycle(true) {
                    ed.run(cmd);
                }
                return true;
            }
            // Otherwise a lone `CTRL-L` in Insert with no active completion — fall through (unbound, no-op).
        }
        false
    }

    /// `H`/`M`/`L`, alone or under an operator: whether `key` was one.
    pub(super) fn screen_line_owns(
        &mut self,
        key: KeyEvent,
        at: KeyCtx,
        frame: &FrameCtx,
        focused_h: usize,
    ) -> bool {
        let so = self.scrolloff_rows();
        let Session {
            ed: Editing { ws, .. },
            line_idx,
            engine,
            folds,
            ..
        } = self;
        let KeyCtx {
            chord, cancels_op, ..
        } = at;
        // `H` / `M` / `L` — the top / middle / bottom visible line (first non-blank via GotoLine). These are
        // viewport-dependent, so they stay a FRONTEND intercept: the session resolves the target buffer line
        // from the current viewport, then hands it to the engine, which composes it with any PENDING OPERATOR
        // over `GotoLine` (`dH`/`yL`/`c2M`/`>H`/`gUL`…). Operator-pending is the `op` axis WITHIN Normal mode
        // (not a distinct crossterm mode), so `normal` is still true after `d`/`c`/`y`/… and this intercept
        // fires for the operator case too — `screen_op` returns the operator command when armed, else the bare
        // move, preserving plain `H`/`M`/`L`. Under an operator `GotoLine` is LINEWISE in core (`:help H`), and
        // its range is `[min,max]` of the cursor and target lines, so direction is handled for free. While the
        // engine waits for an argument (`fH`, `rL`, `gM`, `]M`) the letter is that argument instead (#679).
        if (chord || cancels_op)
            && !key
                .modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('H' | 'M' | 'L'))
        {
            let motion = match key.code {
                KeyCode::Char('H') => ScreenMotion::ViewHigh,
                KeyCode::Char('M') => ScreenMotion::ViewMiddle,
                _ => ScreenMotion::ViewLow,
            };
            // The effective count (`2H` = 2nd line from top; `3d2H` = 6H) resolves the target line; the op
            // itself is consumed by `screen_op`. `screen_count` applies the operator*motion multiplication.
            let count = engine.screen_count();
            // Under an operator Vim IGNORES 'scrolloff' — `dH`/`dL` operate through the TRUE top/bottom visible
            // line (verified against nvim), whereas the bare cursor-motion keeps the scrolloff margin.
            let scrolloff = if engine.has_op() { 0 } else { so };
            let fv = focused_folds(folds, ws);
            let row = viewport_line(motion, count, scrolloff, ws, frame, line_idx, fv, focused_h);
            match engine.screen_op(motion, count, row as u32 + 1) {
                Feed::Cmd(cmd) => self.ed.run(cmd),
                // `!H` / `!L` / `!M`: the filter over the screen line opens its pre-seeded `:{range}!` line,
                // exactly like `!{motion}` below (#800) — it was dropped, so the command was typed as text.
                Feed::FilterMotion { count, motion } => {
                    if let Some((first, last)) = ws.reindent_range(motion, count) {
                        engine.open_filter_cmdline(&format!("{},{}!", first + 1, last + 1));
                    }
                }
                _ => {}
            }
            return true;
        }
        false
    }

    /// Run a scroll key on the focused window ([`run_scroll`]) with the session's own state.
    fn scroll_key(&mut self, key: ScrollKey, frame: &FrameCtx, height: usize) {
        let so = self.scrolloff_rows();
        let Session {
            ed,
            line_idx,
            folds,
            screen_want,
            scroll_valid,
            ..
        } = self;
        let fv = focused_folds(folds, &ed.ws);
        run_scroll(
            key,
            ed,
            frame,
            line_idx,
            fv,
            so,
            height,
            screen_want,
            scroll_valid,
        );
    }
}

/// Dispatch the key after a `C-w` prefix (F-007 MVP window commands): `w`/`C-w` focus next, `s` split
/// horizontally, `v` split vertically, `c` close focused, `q` = `:quit`. An unrecognised key is ignored
/// (Vim beeps). The full `C-w` family is post-MVP.
fn dispatch_window(
    key: crossterm::event::KeyEvent,
    count: u32,
    ws: &mut Workspace,
    window_ex: &mut Option<&'static str>,
) {
    // `{count}C-w w` / `{count}C-w W` go to window `count`, the last one when there are fewer (nvim 0.12.6:
    // `9<C-w>w` with three windows lands in window 3); without a count they step to the next / previous.
    let nth = |ws: &mut Workspace| {
        let n = (count as usize).min(ws.window_count());
        ws.focus_window(n - 1);
    };
    match key.code {
        KeyCode::Char('w' | 'W') if count > 0 => nth(ws),
        _ if is_ctrl(key, 'w') && count > 0 => nth(ws),
        _ if is_ctrl(key, 'w') => ws.focus_next(),
        KeyCode::Char('w') if key.modifiers.is_empty() => ws.focus_next(),
        KeyCode::Char('W') => ws.focus_prev(),
        KeyCode::Char('s') => {
            ws.split(SplitDir::Horizontal);
        }
        KeyCode::Char('v') => {
            ws.split(SplitDir::Vertical);
        }
        // `C-w c` is `:close` and `C-w q` (`C-w C-q`) is `:quit` (`:help CTRL-W_c`): they run as those Ex
        // commands once routing is done, so 'hidden', 'autowrite' and 'confirm' apply as they do (#619).
        KeyCode::Char('c') if key.modifiers.is_empty() => *window_ex = Some("close"),
        KeyCode::Char('q') if key.modifiers.is_empty() => *window_ex = Some("quit"),
        _ if is_ctrl(key, 'q') => *window_ex = Some("quit"),
        _ => {}
    }
}
