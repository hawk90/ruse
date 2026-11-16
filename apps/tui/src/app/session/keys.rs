//! The owners `Session::route_key` offers a key to, before and after the Normal intercepts (#1354): the
//! macro layer, the completion popup, the prompts, an open picker, a terminal buffer, and last the input
//! engine. Each borrows the `Session` fields it uses, as `route_key` did.

use super::normal_keys::focused_folds;
use super::*;

impl Session {
    /// The macro layer's turn at a key: under the Vim profile an Alt key is first split into `<Esc>` and
    /// the key; then the macro state machine records, stops or replays. `Some` is the key that goes on to
    /// the other owners; `None` when the macro layer took it.
    pub(super) fn macro_key(&mut self, raw_key: KeyEvent, from_replay: bool) -> Option<KeyEvent> {
        let overlay_open = self.overlay_open();
        let Session {
            ed:
                Editing {
                    ws,
                    recorded,
                    status,
                    ..
                },
            emacs_profile,
            engine,
            prefix,
            macros,
            last_ex_line,
            profile_name,
            meta_tail,
            ..
        } = self;
        // An Alt/Meta key under the Vim profile is `<Esc>` then the key, as in nvim v0.12.6 for an unmapped
        // `<M-x>` (ruse has no Alt mappings in Vim). Typing `<Esc>x` fast, or over a slow link, delivers both
        // bytes in one read, which the terminal decoder reports as Alt-x (#896).
        let raw_key = if !from_replay
            && *profile_name == "vim"
            && raw_key.modifiers.contains(KeyModifiers::ALT)
        {
            let mut tail = raw_key;
            tail.modifiers -= KeyModifiers::ALT;
            *meta_tail = Some(tail);
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
        } else {
            raw_key
        };
        // Run the key through the macro state machine (capture / stop / `q`|`@` prefixes). Only a
        // `Dispatch` falls through to the normal engine + intercepts; the rest are handled here. `q`, `@`
        // and `Q` are commands only at a command boundary (#679, #680, #851): not while an overlay or a
        // session prefix (`C-w`, `z`, …) owns the key, not in Insert or on the command line, and not while
        // the engine waits for one (`fq`, `"q`, `gq`, `dq`, a Native leader key), where the `q` or `@` is
        // that key. `q`, `@` and `Q` also work from Visual (nvim 0.12.6, #1068), but not from Select.
        let boundary =
            !overlay_open && matches!(prefix, Prefix::None) && engine.pending() == Pending::Base;
        let mode = ws.focused().view.mode();
        // Not under the modeless Emacs profile, whose `q` and `@` self-insert (#1156).
        let macro_cmd =
            boundary && !*emacs_profile && matches!(mode, Mode::Normal | Mode::Visual { .. });
        let step = macros.step_gated(raw_key, from_replay, macro_cmd, macro_cmd);
        // A count typed before `q` (record or stop), `Q` with nothing recorded, or an aborted register name
        // is discarded, as in nvim (`3qaxq` deletes one char); only an armed `@` keeps it for its replay.
        if matches!(
            step,
            crate::keys::Step::Consumed
                | crate::keys::Step::Store(..)
                | crate::keys::Step::OpenCmdWin(_)
        ) && !macros.awaits_replay_register()
        {
            engine.take_count();
        }
        let key = match step {
            crate::keys::Step::Dispatch(k) => k,
            crate::keys::Step::Consumed => return None,
            crate::keys::Step::NoPreviousRegister => {
                engine.take_count();
                status.error("E748: No previously used register");
                return None;
            }
            // `q:` / `q/` / `q?` — open the command-line window (`:help cmdwin`) over that history ring.
            crate::keys::Step::OpenCmdWin(kind) => {
                engine.open_cmdwin(kind);
                return None;
            }
            crate::keys::Step::Store(reg, bytes) => {
                let n = bytes.len();
                // Recorded as the slots it changed (RFC-0020 R6): no recorded command wrote them.
                record_register_writes(ws, recorded, |ws| ws.set_register_raw(Some(reg), bytes));
                status.echo(format!("recorded @{reg} ({n} keys)"));
                return None;
            }
            crate::keys::Step::Replay(reg) => {
                // `{count}@{reg}` repeats the macro `count` times; the count was accumulated by the engine
                // from the digits typed before `@`, so consume it here. The recursion budget is shared
                // across all copies, so `999@a` of a big macro still terminates.
                let n = engine.take_count().max(1);
                let bytes = ws.register_bytes(Some(reg));
                // In linewise Visual nvim runs the register on each selected line instead (`v_@-default`:
                // `:'<,'>normal! @{reg}`), ignoring the count: leave Visual, then one run per line.
                if mode
                    == (Mode::Visual {
                        kind: ruse_core::SelectKind::Linewise,
                    })
                {
                    let pane = ws.focused();
                    let b = pane.doc.bytes();
                    if let Some((BytePos(s), BytePos(e))) = pane.view.selection_span(b) {
                        let first = ruse_core::pos::line_of(b, ruse_core::BytePos(s));
                        let last = ruse_core::pos::line_of(
                            b,
                            ruse_core::BytePos(e.saturating_sub(1).max(s)),
                        );
                        macros.replay(b"\x1b");
                        macros.replay_lines(first, last, bytes);
                    }
                    return None;
                }
                // An empty register replays nothing however large the count (#685).
                if crate::keys::decode(&bytes).is_empty() {
                    return None;
                }
                for _ in 0..n {
                    if !macros.replay(&bytes) {
                        status.warn("macro replay aborted (key limit)");
                        break;
                    }
                }
                return None;
            }
            // `[count]@:` — repeat the last-executed Ex command-line (`:help @:`). We re-enqueue the stored
            // `":`-register line as `:` + text + `<CR>` into the SAME macro-replay queue `@{reg}` uses, so
            // the keys re-drive the whole Ex pipeline (`engine.feed` → `Feed::ExecuteEx` below) exactly as
            // if re-typed — no second Ex executor. `[count]` enqueues that many copies. No prior Ex line is
            // Vim's E30 (a no-op edit-wise); verified vs nvim v0.12.4.
            crate::keys::Step::RepeatEx => {
                let n = engine.take_count().max(1);
                match &last_ex_line {
                    Some(line) => {
                        let mut bytes = Vec::with_capacity(line.len() + 2);
                        bytes.push(b':');
                        bytes.extend_from_slice(line.as_bytes());
                        bytes.push(b'\r');
                        for _ in 0..n {
                            if !macros.replay(&bytes) {
                                status.warn("macro replay aborted (key limit)");
                                break;
                            }
                        }
                    }
                    None => status.error("E30: No previous command line"),
                }
                return None;
            }
        };
        Some(key)
    }

    /// The completion popup and omni-completion keys (F-014): whether they took `key`.
    pub(super) fn popup_owns(&mut self, key: KeyEvent, frame: &FrameCtx) -> bool {
        let Session {
            ed:
                Editing {
                    ws,
                    files,
                    recorded,
                    status,
                    ..
                },
            emacs_profile,
            lsp,
            ..
        } = self;
        let revision = frame.revision;
        let snapshot = &frame.snapshot;
        // F-014 #5: the completion pum owns the keystream while open (nav / accept / dismiss); `<C-x><C-o>`
        // in Vim/Native Insert opens it. Each returns whether it consumed the key (so the loop `continue`s).
        if lsp.on_completion_key(key, ws, status, &mut |ws, ex| run_ex(ex, ws, recorded)) {
            return true;
        }
        if lsp.on_omni_key(key, ws, files, snapshot, revision, *emacs_profile, status) {
            return true;
        }
        false
    }

    /// The `:s///c` confirm loop and the pending prompts: whether one took `key`.
    pub(super) fn prompt_owns(&mut self, key: KeyEvent) -> bool {
        let report = self.ui_opts.report;
        let Session {
            ed:
                Editing {
                    ws,
                    recorded,
                    status,
                    ..
                },
            confirm,
            swap,
            changed_prompt,
            emacs_prompt,
            swap_answer,
            ..
        } = self;
        // A `:s///c` confirm loop owns the keystream while active: y/n/a/l/q per match (F-009 #2).
        if confirm.is_some() {
            confirm_key(confirm, key, ws, recorded, status, report);
            return true;
        }
        // A backwards-range prompt owns the next key; [`Session::on_key`] answers it once routing is done.
        if swap.is_some() || changed_prompt.is_some() || emacs_prompt.is_some() {
            *swap_answer = Some(key);
            return true;
        }
        false
    }

    /// An open picker: whether it took `key`, and the action an accepted item runs.
    pub(super) fn picker_owns(&mut self, key: KeyEvent, frame: &FrameCtx) -> bool {
        let Session {
            ed:
                Editing {
                    ws,
                    files,
                    recorded,
                    status,
                    ..
                },
            highlighters,
            overlay,
            lsp,
            journal_writers,
            journal_failed,
            ..
        } = self;
        // A picker owns the keystream while open (F-004 / F-013 NAT-3): its transient keymap filters/moves,
        // Enter accepts (the session runs the payload-specific action below), Esc closes.
        if let Some(open) = overlay.as_mut() {
            let (outcome, accepted) = open.on_key(key);
            if !matches!(outcome, PickOutcome::Continue) {
                *overlay = None;
            }
            match accepted {
                None => {}
                // The line, diagnostics, marks and position pickers jump the cursor to a byte offset.
                Some(Accepted::Jump(offset)) => place_cursor(ws, recorded, offset),
                // The buffer picker switches the focused window to the selected buffer.
                Some(Accepted::Buffer(id)) => {
                    ws.focus_buffer(id);
                }
                // The file picker opens the selected path into a buffer (the interactive form of `:e`).
                Some(Accepted::File(p)) => {
                    status.give(
                        open_file_into_buffer(&p.display().to_string(), ws, files, highlighters)
                            .status,
                    );
                }
                // F-014: the references picker jumps to the selected location. The `spans` borrow is
                // released by now (render is done), so opening a cross-file buffer here is free — no defer
                // needed (unlike the response-loop goto). The open helper focuses the buffer already
                // holding this file (#848), so the current file and another open file both move without a
                // second Document.
                Some(Accepted::Location(uri, l, c)) => {
                    let path = crate::lsp::uri_to_path(&uri);
                    let target = std::fs::canonicalize(&path).unwrap_or(path);
                    let before = ws.focused_buffer();
                    let opened = open_file_into_buffer(
                        &target.display().to_string(),
                        ws,
                        files,
                        highlighters,
                    );
                    if opened.id.is_some() {
                        let bytes = ws.focused().doc.bytes();
                        let at = crate::lsp::model::lsp_pos_to_byte(bytes, l, c);
                        // A move in the same buffer is recorded (#1134); a trace does not follow a switch
                        // to another buffer, so a placement there is not.
                        if ws.focused_buffer() == before {
                            place_cursor(ws, recorded, at);
                        } else {
                            ws.place_focused_cursor(BytePos(at));
                        }
                    } else {
                        status.give(opened.status);
                    }
                }
                // F-014: the code-action picker applies the selected action's WorkspaceEdit (inline, as
                // the `spans` borrow is released post-render — like the references jump).
                Some(Accepted::Action(action)) => {
                    if !action.edit.is_empty() {
                        // An LSP edit batch syncs the journal first (RFC-0023 §4, #1202).
                        sync_journals(ws, files, journal_writers, journal_failed, status);
                    }
                    lsp.apply_code_action(
                        &action,
                        ws,
                        files,
                        highlighters,
                        status,
                        &mut |ws, ex| run_ex(ex, ws, recorded),
                    );
                }
                // The command palette dispatches the selected command by its stable id, through the same
                // path as a typed key's command (so it is resolved, undone and recorded like any other,
                // #689).
                Some(Accepted::Command(cmd)) => self.run_resolved_cmd(cmd, frame),
            }
            return true;
        }
        false
    }

    /// A terminal buffer: whether it took `key`.
    #[cfg(unix)]
    pub(super) fn terminal_owns(&mut self, key: KeyEvent) -> bool {
        let Session {
            ed: Editing { ws, .. },
            prefix,
            #[cfg(unix)]
            terminals,
            ..
        } = self;
        // F-011 terminal routing: a terminal buffer owns the keystream. In Terminal mode keys forward to the
        // PTY child, except `CTRL-\ CTRL-N` which drops to Terminal-Normal. In Terminal-Normal, `i`/`a`/`A`
        // resume Terminal; every other key falls through to the normal grammar (so `:q`, `C-w`, etc. work) —
        // the placeholder document is empty, so slice 1 has no scrollback paging (that arrives with the grid).
        #[cfg(unix)]
        {
            let tid = ws.focused_buffer();
            if terminals.contains_key(&tid) {
                match ws.focused().view.mode() {
                    Mode::Terminal => {
                        if matches!(prefix, Prefix::TermEscape) {
                            *prefix = Prefix::None;
                            if is_ctrl(key, 'n') {
                                self.ed.run(Command::EnterTerminalNormal);
                                return true;
                            }
                            // Not the escape: the swallowed `CTRL-\` is dropped; fall through to send `key`.
                        }
                        if is_ctrl(key, '\\') {
                            *prefix = Prefix::TermEscape;
                            return true;
                        }
                        if let Some(bytes) = pty::encode_key(key) {
                            if let Some(t) = terminals.get_mut(&tid) {
                                t.send(&bytes);
                            }
                        }
                        return true;
                    }
                    // Only the plain keys resume: `<C-a>` / `<C-i>` are not `a` / `i` (#1069). Anything else
                    // falls through to the normal editor grammar (read-only over an empty doc).
                    Mode::TerminalNormal
                        if matches!(key.code, KeyCode::Char('i' | 'a' | 'A'))
                            && !crate::input::is_chord(key) =>
                    {
                        self.ed.run(Command::EnterTerminal);
                        return true;
                    }
                    _ => {}
                }
            }
        }
        false
    }

    /// The input engine's turn: every key no owner took, command-line keys included, and what its `Feed`
    /// asks the session to do.
    pub(super) fn feed_engine(&mut self, key: KeyEvent, frame: &FrameCtx) {
        match self.engine_feed(key, frame) {
            // A finished `:`-line (F-026): parse + run it. `submit_search` already folded a `/`-line
            // into `Feed::Cmd` inside the engine, so the frontend only sees the ex case here.
            Feed::ExecuteEx(text) => self.execute_ex(text, frame),
            Feed::EmacsAction(a) => self.emacs_action(a, frame),
            fed @ (Feed::CmdlineInsertUnder { .. }
            | Feed::CmdlineInsertLine
            | Feed::CmdlineComplete(_)
            | Feed::CmdlineInsertRegister(_)) => self.cmdline_splice(fed),
            fed @ (Feed::FilterMotion { .. } | Feed::FilterSpan(_) | Feed::CountCmdline { .. }) => {
                self.seeded_cmdline(fed);
            }
            // Resolved in `engine_feed`.
            Feed::Pending | Feed::Ignored | Feed::ScreenMotion { .. } => {}
            fed @ (Feed::Beep { .. } | Feed::Cmd(_) | Feed::KeepJumps(_)) => {
                self.run_fed(fed, frame)
            }
            fed @ (Feed::Replay(_) | Feed::Repeat(_)) => self.repeat_fed(fed, frame),
        }
    }

    /// Feed `key` to the input engine and report its error; a screen-line motion (#615) is finished here,
    /// where the window's screen rows are known.
    fn engine_feed(&mut self, key: KeyEvent, frame: &FrameCtx) -> Feed {
        let Session {
            ed: Editing { ws, status, .. },
            line_idx,
            engine,
            folds,
            screen_want,
            ..
        } = self;
        // Every other key — command-line included — goes through the engine (F-026): the command-line
        // namespace owns its buffer, so the frontend no longer special-cases `:`/`/` typing.
        let fed = engine.feed(key, ws.focused().view.mode());
        if let Some(e) = engine.take_error() {
            status.error(e.to_string());
        }
        // A screen-line motion (#615) lands where the window's screen rows say; the engine finishes it.
        let fed = match fed {
            Feed::ScreenMotion { motion, count } => {
                let fv = focused_folds(folds, ws);
                resolve_screen_motion(motion, count, ws, engine, frame, line_idx, fv, screen_want)
            }
            fed => fed,
        };
        fed
    }

    /// The command-line splices: the word or line under the cursor, a register, a completion (F-026, #613).
    fn cmdline_splice(&mut self, fed: Feed) {
        let Session {
            ed:
                Editing {
                    ws,
                    files,
                    recorded,
                    ..
                },
            engine,
            fold_method,
            foldlevel,
            alt_file,
            ..
        } = self;
        match fed {
            // `c_CTRL-R_CTRL-W` / `c_CTRL-R_CTRL-A`: splice the `<cword>` / `<cWORD>` under the BUFFER
            // cursor into the open command line. The engine owns the cmdline buffer but has no document, so
            // resolve the word here and hand it back. No string under the cursor → nothing spliced (nvim
            // inserts nothing and stays on the line). An incsearch preview refreshes on the next redraw,
            // which reads the updated `engine.cmdline()`.
            Feed::CmdlineInsertUnder { big } => {
                let word = if big {
                    ws.cbig_word_under_cursor()
                } else {
                    ws.cword_under_cursor()
                };
                if let Some(w) = word {
                    engine.cmdline_splice(&w);
                }
            }
            // `c_CTRL-R_CTRL-L`: the buffer line under the cursor, without its newline.
            Feed::CmdlineInsertLine => {
                let b = ws.focused().doc.bytes();
                let line = ruse_core::pos::line_of(b, ws.focused().view.cursor());
                let start = ruse_core::pos::nth_line_start(b, line).0.min(b.len());
                let end = b[start..]
                    .iter()
                    .position(|&c| c == b'\n')
                    .map_or(b.len(), |n| start + n);
                let text = String::from_utf8_lossy(&b[start..end]).into_owned();
                engine.cmdline_splice(&text);
            }
            // `<Tab>`, `<S-Tab>`, `c_CTRL-D`/`CTRL-L`/`CTRL-A` (#613): complete with what the workspace knows
            // (buffer names, `%`/`#`, option values); the engine owns the line and the wildmenu.
            Feed::CmdlineComplete(key) => {
                let buf = ws.focused_buffer();
                let ctx = ExCompletion {
                    ws,
                    files,
                    alt_file,
                    fold_method: fold_method.get(&buf).copied(),
                    foldlevel: foldlevel.get(&buf).copied(),
                };
                engine.cmdline_complete(key, &ctx);
            }
            // `c_CTRL-R {reg}`: the register's text at the caret. The read-only `"/ ": ". "%` registers are
            // refreshed first, as before any dispatch. An unknown or empty register splices nothing.
            Feed::CmdlineInsertRegister(name) => {
                sync_special_registers(
                    ws,
                    recorded,
                    engine,
                    &self.last_ex_line,
                    files,
                    &self.alt_file,
                );
                let name = name.to_ascii_lowercase();
                let linewise = ws
                    .register_snapshot()
                    .iter()
                    .any(|(n, kind, _)| *n == name && *kind == ruse_core::RegKind::Linewise);
                let bytes = ws.register_bytes(Some(name));
                engine.cmdline_splice_register(&String::from_utf8_lossy(&bytes), linewise);
            }
            _ => unreachable!("routed by feed_engine"),
        }
    }

    /// The `:` lines the engine opens pre-seeded with a range: `!{motion}`, `!` over a search or mark, and
    /// `{count}:`.
    fn seeded_cmdline(&mut self, fed: Feed) {
        let Session {
            ed: Editing { ws, status, .. },
            engine,
            search_hl,
            ..
        } = self;
        match fed {
            // `!{motion}` / `!!` (the filter operator): the engine resolved a linewise motion but has no
            // document to number it. Resolve the op-span's first/last line here, then open the `:` cmdline
            // pre-seeded with `{first},{last}!` (Vim's `:.,.+N!`). Typing the command and `<CR>` runs it
            // through the SAME `:{range}!{cmd}` ex-filter path (parse_ex → Ex::Filter → shell.rs). An empty
            // span (nothing to filter) opens nothing, matching Vim leaving Normal untouched.
            Feed::FilterMotion { count, motion } => {
                if let Some((first, last)) = ws.reindent_range(motion, count) {
                    // `reindent_range` yields 0-based inclusive lines; the ex range is 1-based.
                    engine.open_filter_cmdline(&format!("{},{}!", first + 1, last + 1));
                }
            }
            // `!` over a search, `n`/`N`, `*`/`#`, `gn` or a mark (#800): the planner measures the span the
            // command's operator would cover; no target (no match, unset mark) opens nothing, as in nvim.
            Feed::FilterSpan(cmd) => {
                if !note_word_search(&cmd, ws, engine, search_hl, status) {
                    return;
                }
                // A search (`/pat`, `?pat`, `n`, `N`, bare, under an operator or `!`) whose pattern matches
                // nowhere fails with E486 and nothing runs (#901); with no previous pattern, E35 (#1173).
                if search_misses(&cmd, ws, status) {
                    return;
                }
                if no_previous_pattern(&cmd) {
                    status.error(E35);
                    return;
                }
                if let Some((first, last)) = ws.span_lines(&cmd) {
                    engine.open_filter_cmdline(&format!("{},{}!", first + 1, last + 1));
                }
            }
            // `{count}:` — the line opens on the cursor's line and the `count - 1` below it, numbered here.
            // nvim seeds `:.,.+{count-1}` (`:.` for 1); the lines are the same, and a range past the last line
            // is E16 when it runs, as there.
            Feed::CountCmdline { count } => {
                let first =
                    ruse_core::pos::line_of(ws.focused().doc.bytes(), ws.focused().view.cursor())
                        + 1;
                let last = first + count as usize - 1;
                engine.open_filter_cmdline(&if count == 1 {
                    format!("{first}")
                } else {
                    format!("{first},{last}")
                });
            }
            _ => unreachable!("routed by feed_engine"),
        }
    }

    /// A command the engine finished: a beep, a command, or one under `keepjumps`.
    fn run_fed(&mut self, fed: Feed, frame: &FrameCtx) {
        let Session { ed, .. } = self;
        match fed {
            // An unknown key (#1111): record the failure, so a macro or `:normal` stops here.
            Feed::Beep { clear_register } => {
                if clear_register {
                    ed.run(Command::SetRegister(None));
                }
                ed.ws.beep();
            }
            // A change that fails before nvim saves its redo (`d/zzz<CR>`, `dfz`, `d'a` unset) is no change
            // for `.`: the engine recorded it before the core could tell (#1179). The beep is kept for the
            // macro and `:normal` abort read after the key.
            Feed::Cmd(cmd) => {
                let beeped_before = self.ed.ws.take_beep();
                self.ed.ws.take_refused();
                self.run_resolved_cmd(cmd.clone(), frame);
                let beeped = self.ed.ws.take_beep();
                // A refused register beeps after nvim saved the redo; a failed motion before (#1184).
                let refused = self.ed.ws.take_refused();
                if beeped {
                    self.engine.change_failed(&cmd, refused);
                }
                if beeped_before || beeped {
                    self.ed.ws.beep();
                }
            }
            // `` g`a `` / `g'a` (#1175): the jump runs under `keepjumps`, switched through the trace (RFC-0020
            // R6) as `:keepjumps` is, so a replay skips the jumplist entry too. Already on (`:keepjumps normal
            // g`a`), it stays on.
            Feed::KeepJumps(cmd) => {
                let kept = self.ed.ws.keep_jumps();
                if !kept {
                    run_ex(
                        ExCmd::KeepJumps(true),
                        &mut self.ed.ws,
                        &mut self.ed.recorded,
                    );
                }
                self.run_resolved_cmd(cmd, frame);
                if !kept {
                    run_ex(
                        ExCmd::KeepJumps(false),
                        &mut self.ed.ws,
                        &mut self.ed.recorded,
                    );
                }
            }
            _ => unreachable!("routed by feed_engine"),
        }
    }

    /// `.` and the other repeats: each replayed command is recorded and applied, so the trace (F-022)
    /// captures the resolved edit, not the key.
    fn repeat_fed(&mut self, fed: Feed, frame: &FrameCtx) {
        let report = self.ui_opts.report;
        let Session {
            ed,
            line_idx,
            engine,
            folds,
            last_ex_line,
            alt_file,
            ..
        } = self;
        let snapshot = &frame.snapshot;
        match fed {
            // `.` (dot-repeat) replays the last change; record and apply each concrete command so the
            // trace (F-022) captures the resolved edit, not the `.` keypress.
            Feed::Replay(cmds) => {
                sync_special_registers(
                    &mut ed.ws,
                    &mut ed.recorded,
                    engine,
                    last_ex_line,
                    &ed.files,
                    alt_file,
                );
                // `.` over a closed fold acts on it as the typed command did (#819): its first command
                // meets the buffer as it is now; the rest follow from where that one leaves it.
                let fv = focused_folds(folds, &ed.ws);
                let mut cmds = cmds.into_iter();
                let first = cmds.next().and_then(|c| {
                    crate::app::fold_cmds::prepare(
                        c,
                        &mut ed.ws,
                        fv,
                        line_idx,
                        snapshot,
                        &mut ed.recorded,
                    )
                });
                // Python re-indents on the replayed keys as on the typed ones (nvim's redo re-types them).
                let can = &mut self.can_cindent;
                let highlighters = &self.highlighters;
                if let Some(first) = first {
                    for cmd in std::iter::once(first).chain(cmds) {
                        let after =
                            crate::app::indent_keys::before(&cmd, &ed.ws, highlighters, can);
                        ed.run_reported(cmd, report);
                        if let Some(fix) =
                            crate::app::indent_keys::after(after, &ed.ws, highlighters)
                        {
                            ed.run(fix);
                        }
                    }
                }
            }
            Feed::Repeat(r) => {
                sync_special_registers(
                    &mut ed.ws,
                    &mut ed.recorded,
                    engine,
                    last_ex_line,
                    &ed.files,
                    alt_file,
                );
                run_repeat(
                    r,
                    &mut ed.ws,
                    &ed.files,
                    &mut ed.recorded,
                    &mut ed.status,
                    &mut ed.quit,
                    report,
                );
            }
            _ => unreachable!("routed by feed_engine"),
        }
    }
}
