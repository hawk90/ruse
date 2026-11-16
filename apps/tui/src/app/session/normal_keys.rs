//! The session's own Normal, Visual and Insert keys, offered a key before the input engine (#1354): the
//! `z`, `zf` and `C-w` prefixes, the chords at a command boundary, the Insert keys the engine cannot
//! resolve without the buffer, and `H`/`M`/`L`. Each borrows the `Session` fields it uses.

use super::*;

mod normal_chord;

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
            ws,
            line_idx,
            recorded,
            prefix,
            folds,
            status,
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
            ws,
            files,
            line_idx,
            recorded,
            quit,
            folds,
            status,
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
            let cur = line_idx.line_of(ws.focused().view.cursor().0);
            let last = line_idx.line_of(snapshot.len().saturating_sub(1));
            let top = ws.focused().view.top();
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
                    run_cmd(
                        Command::Move(n as u32, m),
                        ws,
                        files,
                        recorded,
                        status,
                        quit,
                    );
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
                place_cursor(ws, recorded, start + off);
            }
            // Over closed folds the rows are counted as nvim counts them (a fold is one row).
            let fv = focused_folds(folds, ws);
            let new_top = match frame.wins.get(ws.focus()) {
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
            ws.set_top(ws.focus(), new_top);
            return true;
        }
        false
    }

    /// `z` + a horizontal scroll verb (`zh`/`zl`/`zH`/`zL`/`zs`/`ze`): whether `key` was one.
    fn z_hscroll(&mut self, key: KeyEvent, count: u32, frame: &FrameCtx) -> bool {
        let Session {
            ws,
            line_idx,
            recorded,
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
        let Session { ws, engine, .. } = self;
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
                    self.run_cmd(cmd);
                    return true;
                }
                KeyCode::Char('y') if visual => {
                    let cmd = Command::VisualOp(ruse_core::VisualOp::YankTrim);
                    self.run_cmd(cmd);
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

    /// `z` + a fold verb, or another `z` command (a no-op, or a beep where nvim beeps too).
    fn z_fold(&mut self, key: KeyEvent, count: u32, frame: &FrameCtx) {
        let Session {
            ws,
            files,
            line_idx,
            recorded,
            quit,
            prefix,
            folds,
            fold_method,
            foldlevel,
            foldenable,
            status,
            ..
        } = self;
        let snapshot = &frame.snapshot;
        // Fold verbs (manual folds; F-007 carve-out). `zf` folds the Visual selection; the rest act on the fold at the cursor.
        let buf = ws.focused_buffer();
        let cur_line = line_idx.line_of(ws.focused().view.cursor().0);
        let fv = folds.entry(buf).or_default();
        match key.code {
            // `zf` — create a CLOSED fold. From a Visual selection: fold the selection's lines. From
            // Normal mode: arm the `zf{motion}` sub-state (handled at the top of the loop), which folds
            // the range the following motion covers (slice 2).
            // Under an indent 'foldmethod' `zf` is `E350` at once and no operator: the keys after it
            // run as typed (nvim's `foldManualAllowed`), and Visual mode stays.
            KeyCode::Char('f')
                if fold_method.get(&buf).copied() == Some(crate::input::FoldMethod::Indent) =>
            {
                status.error(E350);
            }
            KeyCode::Char('f') => {
                if let Some((BytePos(s), BytePos(e))) = ws.focused().view.selection_span(snapshot) {
                    let (a, b) = (line_idx.line_of(s), line_idx.line_of(e.saturating_sub(1)));
                    let (start, end) = (a.min(b), a.max(b));
                    run_cmd(Command::EnterNormal, ws, files, recorded, status, quit);
                    crate::folds::insert_closed_fold(fv, start, end);
                    place_cursor(ws, recorded, line_idx.nth_line_start(start));
                    status.echo(format!("folded {} lines", end - start + 1));
                } else {
                    // Normal mode: begin `zf{motion}`. Seed the buffer with any count typed before `z`
                    // (e.g. `3zfj`); further count digits typed after `f` are appended by the handler.
                    *prefix = Prefix::Zf(if count > 0 {
                        count.to_string()
                    } else {
                        String::new()
                    });
                }
            }
            // `zo`/`zc`/`za`, `zO`/`zC`/`zA`, `zv`, `zx`/`zX` over nested folds (#1141), as nvim's
            // `setManualFold` walks the folds holding the cursor line from the outermost: `zo` opens
            // the outermost closed one (`{count}` levels), `zc` closes the innermost open one above the
            // first closed one, `zO` opens the outermost closed one and all inside it, `zC` closes
            // every one, `za`/`zA` open when a closed fold hides the line and close otherwise. No fold
            // at the line is `E490`. The cursor keeps its line inside a closed fold (nvim; #819).
            KeyCode::Char(c @ ('o' | 'c' | 'a' | 'O' | 'C' | 'A')) => {
                let n = count.max(1);
                let closed = crate::folds::is_closed_at(fv, cur_line);
                let open = match c {
                    'o' | 'O' => true,
                    'c' | 'C' => false,
                    _ => closed,
                };
                let found = match (open, c.is_ascii_uppercase()) {
                    (true, true) => crate::folds::open_rec(fv, cur_line),
                    (false, true) => crate::folds::close_rec(fv, cur_line),
                    (true, false) => {
                        (0..n).fold(None, |f, _| f.or(crate::folds::open_one(fv, cur_line)))
                    }
                    (false, false) => {
                        (0..n).fold(None, |f, _| f.or(crate::folds::close_one(fv, cur_line)))
                    }
                };
                if found.is_none() {
                    status.error("E490: No fold found");
                }
            }
            // `zj` / `zk` — move the cursor DOWN to the next fold's start / UP to the previous fold's
            // end (`{count}` repeats). A closed fold counts as one: the folds inside it are not
            // stepped to. Cursor-only, column 0 — matching nvim, which ignores 'startofline' here
            // (verified: `zj` lands at col 0 even with `startofline` on). Landing inside a closed fold
            // keeps the cursor there; it paints on the summary row.
            KeyCode::Char('j') | KeyCode::Char('k') => {
                let n = count.max(1);
                let down = matches!(key.code, KeyCode::Char('j'));
                let mut line = cur_line;
                let mut moved = false;
                for _ in 0..n {
                    let next = if down {
                        crate::folds::next_fold_start(fv, line)
                    } else {
                        crate::folds::prev_fold_end(fv, line)
                    };
                    match next {
                        Some(l) => {
                            line = l;
                            moved = true;
                        }
                        None => break, // no more folds that way — keep the last landing
                    }
                }
                if moved {
                    place_cursor(ws, recorded, line_idx.nth_line_start(line));
                }
            }
            // `zR`/`zM`/`zr`/`zm` set 'foldlevel' (the deepest nesting, 0, one more, one less; `{count}`
            // levels for `zr`/`zm`) and `zx`/`zX` re-apply it: a fold deeper than the level is closed,
            // the rest open (nvim's `newFoldLevel`, manual and indent folds alike). `zx` then opens the
            // folds holding the cursor line, as `zv`.
            KeyCode::Char(c @ ('R' | 'M' | 'r' | 'm' | 'x' | 'X')) => {
                let maxl = crate::folds::max_level(fv);
                let cur = foldlevel.get(&buf).copied().unwrap_or(0);
                let n = count.max(1) as usize;
                let new = match c {
                    'R' => maxl,
                    'M' => 0,
                    'r' => cur.saturating_add(n).min(maxl),
                    'm' => cur.saturating_sub(n),
                    _ => cur,
                };
                foldlevel.insert(buf, new);
                let en = foldenable.get(&buf).copied().unwrap_or(true);
                crate::folds::apply_foldlevel(fv, if en { new } else { usize::MAX });
                if c == 'x' {
                    crate::folds::open_cursor_fold(fv, cur_line);
                }
            }
            // `zd` deletes the innermost fold at the cursor line (or the closed one hiding it) and moves
            // the folds inside it up a level; `zD` deletes those too. `zE` deletes every fold. Folds an
            // indent 'foldmethod' makes cannot be deleted (`E351`/`E352`).
            KeyCode::Char(c @ ('d' | 'D' | 'E')) => {
                let indent =
                    fold_method.get(&buf).copied() == Some(crate::input::FoldMethod::Indent);
                if indent {
                    status.error(if c == 'E' {
                        "E352: Cannot erase folds with current 'foldmethod'"
                    } else {
                        "E351: Cannot delete fold with current 'foldmethod'"
                    });
                } else if c == 'E' {
                    crate::folds::eliminate_all(fv);
                } else if !crate::folds::delete_at(fv, cur_line, c == 'D') {
                    status.error("E490: No fold found");
                }
            }
            // `zv` — view cursor line: open just enough folds to reveal the cursor line. Keeps the
            // folds, unlike `zd`.
            KeyCode::Char('v') => {
                crate::folds::open_cursor_fold(fv, cur_line);
            }
            // A `z` command nvim has but ruse lacks (`zh`, `zs`, `zx`, spelling, …) stays a no-op; one
            // nvim lacks too beeps there (`nv_zet` → `clearopbeep`), stopping a macro or `:normal`
            // (#1111).
            KeyCode::Char(c)
                if key.modifiers.is_empty()
                    && !"abcdefghijklmnoprstuvwxyzACDEFGHLMNOPRWX0123456789+-.^=".contains(c) =>
            {
                ws.beep();
            }
            _ => {}
        }
    }

    /// A `C-w` window prefix: whether it took `key` (a count digit, or the window command).
    pub(super) fn window_prefix_owns(&mut self, key: KeyEvent) -> bool {
        let Session {
            ws,
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
        let (ws, engine) = (&self.ws, &self.engine);
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
    /// `CTRL-N`/`CTRL-P` and `CTRL-X CTRL-L`. Whether one took `key`.
    pub(super) fn insert_chord_owns(
        &mut self,
        key: KeyEvent,
        frame: &FrameCtx,
        focused_h: usize,
    ) -> bool {
        let Session {
            ws,
            files,
            recorded,
            emacs_profile,
            engine,
            quit,
            status,
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
        let vim_insert = !*emacs_profile && matches!(ws.focused().view.mode(), Mode::Insert);
        if vim_insert && (is_ctrl(key, 'e') || is_ctrl(key, 'y')) && engine.insert_plain_text_ctx()
        {
            // An active completion cycle owns the key first: `CTRL-E` cancels it, `CTRL-Y` accepts (#805).
            if let Some(feed) = engine.complete_end(is_ctrl(key, 'e')) {
                if let Feed::Cmd(cmd) = feed {
                    run_cmd(cmd, ws, files, recorded, status, quit);
                }
                return true;
            }
            let ch = ws.adjacent_line_char(is_ctrl(key, 'y'));
            if let Feed::Cmd(cmd) = engine.insert_copy_char(ch) {
                run_cmd(cmd, ws, files, recorded, status, quit);
            }
            return true;
        }
        // `i_<PageDown>` / `i_<PageUp>` / `i_<S-Down>` / `i_<S-Up>` — page scroll from Insert, a caret jump
        // that breaks undo and restarts the `.` record like `i_<Down>` (nvim v0.12.6).
        if let (true, Some(forward)) = (vim_insert, insert_page_key(key)) {
            if engine.insert_plain_text_ctx() {
                // The jump breaks undo first (it changes no text, so the scroll below sees the same lines).
                run_cmd(engine.insert_page_jump(), ws, files, recorded, status, quit);
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
        if vim_insert && (is_ctrl(key, 'n') || is_ctrl(key, 'p')) && engine.insert_plain_text_ctx()
        {
            let forward = is_ctrl(key, 'n');
            let feed = if engine.completion_active() {
                engine.complete_cycle(forward)
            } else {
                let (base, cands) = ws.keyword_completion();
                engine.complete_start(base, cands, forward)
            };
            if let Feed::Cmd(cmd) = feed {
                run_cmd(cmd, ws, files, recorded, status, quit);
            }
            return true;
        }
        // `i_CTRL-X CTRL-L` (Insert mode) — whole-line completion from the CURRENT buffer (F-003). `CTRL-X`
        // armed the submode in the engine (`insert_ctrl_x_pending`); this `CTRL-L` resolves the candidate
        // lines here (the engine has no buffer — the same split as `i_CTRL-N`) and starts the cycle. A bare
        // `CTRL-L` while a cycle is already active steps it FORWARD (nvim's whole-line continuation key);
        // `CTRL-N`/`CTRL-P` cycle it too (the block above). Deferred: `CTRL-X CTRL-N/CTRL-P` (local keyword)
        // and `CTRL-X CTRL-F` (filename) sources, and the popup menu — all out of scope for this slice.
        if vim_insert && is_ctrl(key, 'l') {
            if engine.insert_ctrl_x_pending() {
                let (base, cands) = ws.line_completion();
                if let Feed::Cmd(cmd) = engine.complete_line_start(base, cands) {
                    run_cmd(cmd, ws, files, recorded, status, quit);
                }
                return true;
            }
            if engine.completion_active() && engine.insert_plain_text_ctx() {
                if let Feed::Cmd(cmd) = engine.complete_cycle(true) {
                    run_cmd(cmd, ws, files, recorded, status, quit);
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
            ws,
            line_idx,
            engine,
            folds,
            ..
        } = self;
        let snapshot = &frame.snapshot;
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
            let to = match key.code {
                KeyCode::Char('H') => viewport::ScreenTo::High,
                KeyCode::Char('M') => viewport::ScreenTo::Middle,
                _ => viewport::ScreenTo::Low,
            };
            // The effective count (`2H` = 2nd line from top; `3d2H` = 6H) resolves the target line; the op
            // itself is consumed by `screen_op`. `screen_count` applies the operator*motion multiplication.
            let count = engine.screen_count();
            // The last real line: the slot after a final newline is no line (#677), so `M` on a short
            // buffer takes the middle of its lines, not of one more.
            let last_line = line_idx.line_of(
                ruse_core::motion::off_phantom(snapshot, ruse_core::BytePos(snapshot.len())).0,
            );
            // Under an operator Vim IGNORES 'scrolloff' — `dH`/`dL` operate through the TRUE top/bottom visible
            // line (verified against nvim), whereas the bare cursor-motion keeps the scrolloff margin.
            let scrolloff = if engine.has_op() { 0 } else { so };
            // The lines that show whole: fewer than the rows when lines wrap (#615).
            let top = ws.focused().view.top();
            let shown = match frame.wins.get(ws.focus()) {
                Some(win) => {
                    let fv = focused_folds(folds, ws);
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
                    if fv.iter().any(|f| f.closed) {
                        // A closed fold is one screen line (#1141): pick among the lines shown.
                        let (vis, at_end) = viewport::shown_lines(&lines, top, focused_h);
                        // Indices into `vis`, offset by one when the window does not start at the first
                        // line, so `H` keeps its 'scrolloff' margin there.
                        let t0 = usize::from(top > 0);
                        let last = t0 + if at_end { vis.len() - 1 } else { vis.len() };
                        let i = viewport::screen_line(t0, vis.len(), scrolloff, to, count, last);
                        Err(vis[(i - t0).min(vis.len() - 1)])
                    } else {
                        let (bot, _) = viewport::botline(&lines, top, focused_h);
                        Ok(if bot >= viewport::Lines::count(&lines) {
                            focused_h
                        } else {
                            (bot - top).max(1)
                        })
                    }
                }
                None => Ok(focused_h),
            };
            let row = match shown {
                Ok(shown) => viewport::screen_line(top, shown, scrolloff, to, count, last_line),
                Err(row) => row,
            };
            match engine.screen_op(row as u32 + 1) {
                Feed::Cmd(cmd) => self.run_cmd(cmd),
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
            ws,
            files,
            line_idx,
            folds,
            screen_want,
            scroll_valid,
            recorded,
            status,
            quit,
            ..
        } = self;
        let fv = focused_folds(folds, ws);
        run_scroll(
            key,
            ws,
            frame,
            line_idx,
            fv,
            so,
            height,
            screen_want,
            scroll_valid,
            files,
            recorded,
            status,
            quit,
        );
    }
}
