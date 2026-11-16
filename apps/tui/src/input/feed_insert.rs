//! The Insert and Replace key paths (#1356): `feed_insert` with its prefixes (`CTRL-R`, `CTRL-G`,
//! `CTRL-K`, `CTRL-V`, `CTRL-X`) and chords, the insert-session helpers, the completion-cycle API, and
//! `feed_replace`. Moved out of `input/mod.rs` as an `impl InputEngine` block.

use super::*;

impl InputEngine {
    /// Whether the Insert namespace is in a PLAIN-TEXT context right now: not mid a `CTRL-G`/`CTRL-R`
    /// prefix, a `CTRL-K` digraph, a `CTRL-V` literal entry, nor an `i_CTRL-O` one-shot. The frontend
    /// checks this before handling `i_CTRL-E`/`i_CTRL-Y`, so those never steal the key a pending prefix
    /// is waiting for (e.g. a `CTRL-R` register name).
    #[must_use]
    pub fn insert_plain_text_ctx(&self) -> bool {
        !self.insert.ctrl_g
            && !self.insert.ctrl_r
            && !self.insert.ctrl_x
            && self.insert.digraph.is_none()
            && self.insert.literal.is_none()
            && !self.in_one_shot()
    }

    /// Whether the Insert `CTRL-X` completion submode (`i_CTRL-X`) is armed — the previous key was `CTRL-X`
    /// and the engine awaits the source selector. The frontend checks this so `i_CTRL-X CTRL-L` (whole-line
    /// completion) is resolved against the buffer here rather than reaching `feed_insert`.
    #[must_use]
    pub fn insert_ctrl_x_pending(&self) -> bool {
        self.insert.ctrl_x
    }

    /// `i_CTRL-E` / `i_CTRL-Y`: insert the frontend-resolved character directly below / above the caret.
    /// The engine has no buffer, so the frontend resolves the adjacent-line char (see
    /// [`Workspace::adjacent_line_char`](ruse_core::Workspace::adjacent_line_char)) and passes it here;
    /// `None` = no such char (short/absent adjacent line), a Vim no-op. The resulting `InsertChar` is
    /// folded into the in-flight dot-repeat change so `.` replays the RESOLVED LITERAL char (matching
    /// nvim, which repeats the character copied — not a re-resolution against the new line).
    pub fn insert_copy_char(&mut self, ch: Option<char>) -> Feed {
        match ch {
            Some(c) => {
                let out = Feed::Cmd(Command::InsertChar(c));
                self.record(&out, Mode::Insert);
                out
            }
            None => Feed::Ignored,
        }
    }

    /// `i_CTRL-E` / `i_CTRL-Y` while a completion cycle is active (`:help complete_CTRL-E`, nvim
    /// `ins_compl_stop`, #805): either ends the cycle. `CTRL-E` (`cancel`) puts the original text back and
    /// inserts nothing. `CTRL-Y` accepts the selected text; with more than one candidate the popup menu is
    /// showing (`'completeopt'` "menu") and it inserts nothing, with one there is no menu and the key goes
    /// on to insert the char above. `Some` is the consumed key's result; `None` means the caller runs the
    /// key as usual (no cycle was active, or a menu-less `CTRL-Y`).
    pub fn complete_end(&mut self, cancel: bool) -> Option<Feed> {
        let c = self.insert.completion.take()?;
        if cancel {
            let out = Feed::Cmd(Command::CompleteWord {
                back: c.applied,
                text: c.base,
            });
            self.record(&out, Mode::Insert);
            return Some(out);
        }
        (c.cands.len() > 1).then_some(Feed::Pending)
    }

    /// Whether an insert-mode keyword completion cycle (`i_CTRL-N` / `i_CTRL-P`) is currently active. The
    /// frontend checks this to decide whether a fresh `CTRL-N`/`CTRL-P` STARTS a cycle (resolve the base +
    /// candidates from the buffer) or CONTINUES one (just step the index).
    #[must_use]
    pub fn completion_active(&self) -> bool {
        self.insert.completion.is_some()
    }

    /// Start an insert-mode keyword completion cycle (`i_CTRL-N` / `i_CTRL-P`) and take the first step.
    /// The frontend resolves `base` (the keyword before the caret) and `cands` (the buffer keywords that
    /// start with it, in nvim scan order — see [`Workspace::keyword_completion`](ruse_core::Workspace::keyword_completion));
    /// the engine owns the cycling. With NO candidates this is a no-op (Vim bells; [`Feed::Ignored`]) and
    /// no cycle is armed. Otherwise the cycle stops are `[cands…, ORIGINAL]`; `forward` (`CTRL-N`) selects
    /// the first candidate, `!forward` (`CTRL-P`) the last. Returns the [`Command::CompleteWord`] applying
    /// the step, recorded into the insert session so `.` replays it.
    pub fn complete_start(&mut self, base: String, cands: Vec<String>, forward: bool) -> Feed {
        if cands.is_empty() {
            return Feed::Ignored;
        }
        let applied = base.chars().count() as u32;
        // Start parked on the ORIGINAL stop (`idx == cands.len()`); the first `complete_cycle` moves off it
        // to the first (`CTRL-N`) or last (`CTRL-P`) candidate.
        self.insert.completion = Some(Completion {
            idx: cands.len(),
            base,
            cands,
            applied,
        });
        self.complete_cycle(forward)
    }

    /// Start an insert-mode WHOLE-LINE completion cycle (`i_CTRL-X CTRL-L`) and take the first step. Consumes
    /// the `CTRL-X` submode ([`insert_ctrl_x_pending`](Self::insert_ctrl_x_pending)) and reuses the same
    /// [`Completion`] cycle machinery as [`complete_start`](Self::complete_start) — the ONLY difference is
    /// the SOURCE: `base` and `cands` are whole-line (post-indent) text resolved by the frontend via
    /// [`Workspace::line_completion`](ruse_core::Workspace::line_completion). Always steps FORWARD onto the
    /// first matching line (Vim's `CTRL-X CTRL-L`); a bare `CTRL-L` (or `CTRL-N`/`CTRL-P`) then continues the
    /// cycle. With NO candidates this is a no-op ([`Feed::Ignored`]) and no cycle is armed (Vim bells).
    pub fn complete_line_start(&mut self, base: String, cands: Vec<String>) -> Feed {
        self.insert.ctrl_x = false; // the submode's job is done — the cycle owns continuation from here
        self.complete_start(base, cands, true)
    }

    /// Advance (`CTRL-N`) or retreat (`CTRL-P`) the active keyword-completion cycle by one stop and emit the
    /// [`Command::CompleteWord`] that swaps the buffer's completion region to the newly selected text. A
    /// no-op ([`Feed::Ignored`]) if no cycle is active. The step is recorded into the in-flight insert
    /// session (like [`insert_copy_char`](Self::insert_copy_char)), so dot-repeat replays the chain and
    /// reproduces the accepted text literally (matches nvim v0.12.4).
    pub fn complete_cycle(&mut self, forward: bool) -> Feed {
        let Some(c) = self.insert.completion.as_mut() else {
            return Feed::Ignored;
        };
        let len = c.cands.len() + 1; // candidates + the ORIGINAL-text stop
        c.idx = if forward {
            (c.idx + 1) % len
        } else {
            (c.idx + len - 1) % len
        };
        let text = c.text_at(c.idx).to_string();
        let back = c.applied;
        c.applied = text.chars().count() as u32;
        let out = Feed::Cmd(Command::CompleteWord { back, text });
        self.record(&out, Mode::Insert);
        out
    }

    /// `i_CTRL-O` ENDS the in-flight insert change for dot-repeat, exactly as `<Esc>` would (Vim's
    /// `ins_ctrl_o` runs `ins_esc`): the typed text so far becomes `last_change` and the `".` text. An entry
    /// count's LIVE repetition is dropped (Vim zeroes it — `3iab<C-o>h` inserts `ab` once), though `.` of
    /// the ended change still repeats it (`3iab<C-o>0<Esc>j.` inserts `ababab`, nvim). The one-shot
    /// command then classifies as a Normal command, and the first edit after the return opens a fresh `i`
    /// change
    /// (`InsertState::resumed`, consumed in [`record`](Self::record)).
    /// Insert resumed by the frontend after a `:normal` run from `i_CTRL-O` (#1097): the keys ran in
    /// Normal mode, which dropped the Insert state, so mark the return as an `i_CTRL-O` one — the first
    /// edit opens a fresh change.
    pub fn resume_insert_after_one_shot(&mut self) {
        self.insert.resumed = true;
    }

    fn end_insert_change_for_one_shot(&mut self) {
        if let Some(mut rec) = self.recording.take() {
            rec.insert.push(Command::EnterNormal);
            self.last_inserted = rec.insert_body().to_vec();
            self.last_change = Some(rec);
        }
        self.insert.resumed = true;
    }

    /// Feed one key given the current mode.
    /// The Insert namespace (mode is Insert, no one-shot in flight). Two multi-key sequences resolve
    /// before the layer — `CTRL-G u` (undo-break), `i_CTRL-^` (Lang-Arg toggle), `CTRL-O` (one-shot
    /// Normal), `CTRL-G` (prefix) — then the Insert layer binds, else the `open/insert` policy applies.
    /// `i_CTRL-A` / `i_CTRL-@` (`:help i_CTRL-A`): re-insert the text of the MOST RECENT insert session
    /// (Vim's `".` register) at the caret. `leave` selects the `CTRL-@` variant, which is "`CTRL-A` then
    /// `<Esc>`" — it additionally leaves Insert, and does so EVEN when there is no previous insert
    /// (verified vs nvim). With no previous insert the stored body is empty, so plain `CTRL-A` is a clean
    /// no-op (nvim rings the bell — "E29: No inserted text yet").
    ///
    /// The body commands are replayed VERBATIM: replaying them (backspaces and all) reproduces the
    /// RESULTING text of that session, exactly as nvim's keystroke replay does. They are also folded into
    /// any in-flight recording so the re-inserted text rolls into the SESSION's `".` and dot-repeat
    /// (matches nvim). The multi-command [`Feed::Replay`] channel bypasses the recorder, so this fold is
    /// done by hand — the same pattern [`feed_ctrl_v`](Self::feed_ctrl_v) uses.
    fn insert_prev_text(&mut self, leave: bool) -> Feed {
        self.reset();
        if self.last_inserted.is_empty() && !leave {
            // Nothing to insert and no mode change to make: a pure no-op.
            return Feed::Ignored;
        }
        let mut cmds = self.last_inserted.clone();
        if leave {
            cmds.push(Command::EnterNormal);
        }
        // Fold each replayed command into the in-flight recording exactly as the outer `feed` would for a
        // sequence of `Cmd`s — recording the trailing `EnterNormal` (the `CTRL-@` case) also closes the
        // session into `last_change` / `last_inserted`, so the next `".` reflects the re-inserted text.
        for cmd in &cmds {
            self.record(&Feed::Cmd(cmd.clone()), Mode::Insert);
        }
        Feed::Replay(cmds)
    }

    /// An Insert-mode cursor key (`i_<Left>`, `i_<Home>`, …). Vim treats it as the end of one insert and
    /// the start of another (verified vs nvim v0.12.6): the undo sequence breaks, so `u` undoes only what
    /// was typed after the move, and `.` repeats only that text as a plain `i`. The in-flight recording is
    /// therefore restarted here, which also drops a count-on-insert (`3ix<Left>y` inserts `y` once). The
    /// move itself is not recorded: it travels in the recorder-bypassing [`Feed::Replay`].
    fn insert_cursor_move(&mut self, cmd: Command) -> Feed {
        self.restart_insert_change();
        Feed::Replay(vec![Command::BreakUndo, cmd])
    }

    /// End the insert change at a caret jump and start a fresh plain-`i` one (see
    /// [`insert_cursor_move`](Self::insert_cursor_move)).
    fn restart_insert_change(&mut self) {
        self.reset();
        if self.recording.is_some() {
            self.recording = Some(ChangeIntent {
                lead: Command::EnterInsert,
                insert: Vec::new(),
                register: None,
                expr: None,
                entry_count: 1,
            });
        }
    }

    /// An Insert-mode page key (`i_<PageDown>`, `i_<S-Up>`, …), which the frontend scrolls because it needs
    /// the window: it is a caret jump like `i_<Down>`, so it breaks the undo sequence and restarts the `.`
    /// record (nvim v0.12.6). Returns the undo break for the caller to run before the scroll.
    pub fn insert_page_jump(&mut self) -> Command {
        // The jump ends a completion cycle, keeping the selected text (#805).
        self.insert.completion = None;
        self.restart_insert_change();
        Command::BreakUndo
    }

    pub(super) fn feed_insert(&mut self, key: KeyEvent) -> Feed {
        // Any key reaching `feed_insert` is NOT a completion-cycle key (`CTRL-N`/`CTRL-P` are intercepted by
        // the frontend before `feed`), so it ACCEPTS the current candidate and ends the cycle (Vim). The
        // accepted text already sits in the buffer; dropping the state is all that is needed.
        self.insert.completion = None;
        // One-key Insert memories: both are consumed by whatever key comes next.
        let last_typed = self.insert.last_typed.take();
        let keep_undo = std::mem::take(&mut self.insert.keep_undo);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if let Some(feed) = self.insert_prefix(key, ctrl) {
            return feed;
        }
        if ctrl {
            if let Some(feed) = self.insert_chord(key, last_typed) {
                return feed;
            }
        }
        // CTRL aliases for keys the Insert layer binds (nvim v0.12.6): `i_CTRL-C` and `<C-[>` leave Insert
        // like `<Esc>` (`feed` drops a count's repeats for `CTRL-C`), `i_CTRL-H` is `<BS>`, `i_CTRL-J` and
        // `<C-m>` are `<CR>`, `i_CTRL-I` is `<Tab>`. Any other unbound chord inserts nothing (`unmatched`).
        let key = match key.code {
            KeyCode::Char('c' | '[') if ctrl => plain(KeyCode::Esc),
            KeyCode::Char('h') if ctrl => plain(KeyCode::Backspace),
            KeyCode::Char('j' | 'm') if ctrl => plain(KeyCode::Enter),
            KeyCode::Char('i') if ctrl => plain(KeyCode::Tab),
            _ => key,
        };
        // `<Tab>` inserts whitespace to the next tabstop (spaces under `expandtab`, else a hard tab). Without
        // this, Tab falls through to the Insert unmatched policy, which only emits for `Char` keys — so a raw
        // `KeyCode::Tab` would silently do nothing.
        if key.code == KeyCode::Tab {
            return self.action(Command::InsertTab);
        }
        // `CTRL-G U` then `<Left>`/`<Right>`: a step that stays in the undo group and the `.` record.
        if keep_undo
            && key.modifiers.is_empty()
            && matches!(key.code, KeyCode::Left | KeyCode::Right)
        {
            return self.action(Command::InsertCaretStep {
                right: key.code == KeyCode::Right,
            });
        }
        // `<S-Left>`/`<C-Left>` and `<S-Right>`/`<C-Right>` move a word back / forward (across lines);
        // `<C-Home>` goes to the first char of the file, `<C-End>` after the last one. Caret jumps, like the
        // plain arrows (nvim v0.12.6). `<S-Up>`/`<S-Down>` page-scroll in the frontend.
        if key.modifiers == KeyModifiers::SHIFT || key.modifiers == KeyModifiers::CONTROL {
            match key.code {
                KeyCode::Left => {
                    return self.insert_cursor_move(Command::Move(1, Motion::WordBack))
                }
                KeyCode::Right => {
                    return self.insert_cursor_move(Command::Move(1, Motion::WordFwd))
                }
                KeyCode::Home if ctrl => {
                    return self.insert_cursor_move(Command::Move(1, Motion::GotoByte))
                }
                KeyCode::End if ctrl => {
                    self.restart_insert_change();
                    return Feed::Replay(vec![
                        Command::BreakUndo,
                        Command::Move(1, Motion::LastLine),
                        Command::MoveLineEnd,
                    ]);
                }
                _ => {}
            }
        }
        // The unmodified cursor and editing keys (`:help ins-special-special`).
        if key.modifiers.is_empty() {
            if let Some(cmd) = insert_cursor_command(key.code) {
                return self.insert_cursor_move(cmd);
            }
            match key.code {
                // `i_<Del>`: delete the char under the caret; at the end of a line, join the next one. It is
                // part of the insert: one undo step with the typed text, and `.` replays it.
                KeyCode::Delete => return self.action(Command::DeleteForward(1)),
                // `i_<Insert>`: toggle to Replace (`<Insert>` in Replace toggles back).
                KeyCode::Insert => return self.action(Command::EnterReplace),
                _ => {}
            }
        }
        if let Resolved::Bound { value, .. } = self.profile.stack(Ns::Insert).resolve(&key.code) {
            let cmd = value.clone();
            self.reset();
            return Feed::Cmd(cmd);
        }
        let out = self.unmatched(Ns::Insert, key);
        if let (KeyCode::Char(c), Feed::Cmd(Command::InsertChar(_))) = (key.code, &out) {
            self.insert.last_typed = Some(c);
        }
        out
    }

    /// A pending Insert prefix takes this key first: the `CTRL-X` submode, `CTRL-R {reg}`, `CTRL-G {key}`,
    /// a `CTRL-K` digraph, a `CTRL-V` literal. `None` when no prefix claimed the key.
    fn insert_prefix(&mut self, key: KeyEvent, ctrl: bool) -> Option<Feed> {
        // `i_CTRL-X` submode: the in-scope second key `CTRL-L` (whole-line completion) is intercepted by the
        // frontend BEFORE `feed_insert` (it needs the buffer), so any key that REACHES here cancels the
        // submode. A CONTROL-modified key is a would-be submode selector we do not support (the deferred
        // `CTRL-N`/`CTRL-P` local-keyword, `CTRL-F` filename sources): SWALLOW it as a clean cancel rather
        // than inserting a stray char. A non-ctrl key falls through and is processed as normal Insert input
        // (so `CTRL-X` then `<Esc>` still leaves Insert, `CTRL-X` then a letter inserts it).
        if self.insert.ctrl_x {
            self.insert.ctrl_x = false;
            if ctrl {
                self.reset();
                return Some(Feed::Ignored);
            }
        }
        // `CTRL-R` prefix: consume the second key as the register NAME and insert its contents at the caret.
        // Accepts the same names the paste path reads (`"`, `0`–`9`, `-`, `a`–`z`/`A`–`Z`); any other key
        // aborts without inserting. Checked before the layer so the register key never reaches text insertion.
        if self.insert.ctrl_r {
            self.insert.ctrl_r = false;
            // A CONTROL-modified second key is NOT a register name. Vim's `i_CTRL-R_CTRL-W`/`_CTRL-A` insert
            // NOTHING (the word-under-cursor inserts are COMMAND-LINE mode only — `c_CTRL-R_CTRL-W`), and the
            // `C-r C-r`/`C-o`/`C-p` insert-literally variants are not implemented. Treat every ctrl-modified
            // selector as a clean no-op rather than mis-reading the char as a register (previously `C-r C-w`
            // wrongly inserted register `w`, `C-r C-a` register `a`). Verified against nvim v0.12.4.
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                self.reset();
                return Some(Feed::Ignored);
            }
            // `i_CTRL-R=` — the expression register (`:help i_CTRL-R`, the MOST common use): open the
            // expression prompt; on `<CR>` the evaluated result is spliced at the caret. Handled before the
            // stored-register names below.
            if key.code == KeyCode::Char('=') {
                self.reset();
                self.open_expr_prompt(ExprTarget::Insert);
                return Some(Feed::Pending);
            }
            return Some(match key.code {
                KeyCode::Char(c)
                    if c == '"'
                        || c == '-'
                        || c == '+'
                        || c == '*'
                        // The read-only special registers `"/ ": ". "%` (`:help i_CTRL-R`): `C-r /`
                        // inserts the last search pattern, `C-r :` the last Ex line, `C-r .` the last
                        // inserted text, `C-r %` the file name. Resolved by the frontend's `set_special`.
                        || c == '/'
                        || c == ':'
                        || c == '.'
                        || c == '%'
                        || c == '#'
                        || c.is_ascii_alphanumeric() =>
                {
                    self.action(Command::InsertRegister(c))
                }
                _ => {
                    self.reset();
                    Feed::Ignored
                }
            });
        }
        // `CTRL-G` prefix: consume the second key (nvim v0.12.6). `u` breaks the undo group; `U` keeps the
        // NEXT `<Left>`/`<Right>` inside it; `j`/`<Down>`/`CTRL-J` and `k`/`<Up>`/`CTRL-K` move a line down /
        // up to the column where inserting started (a caret jump, like `<Down>`). Anything else aborts the
        // prefix without inserting (Vim beeps). Checked before the layer so the printable path never sees the
        // prefixed key.
        if self.insert.ctrl_g {
            self.insert.ctrl_g = false;
            let only_ctrl = key.modifiers - KeyModifiers::SHIFT == KeyModifiers::CONTROL;
            let line = match key.code {
                KeyCode::Char('j') if !is_chord(key) || only_ctrl => Some(true),
                KeyCode::Char('k') if !is_chord(key) || only_ctrl => Some(false),
                KeyCode::Down if key.modifiers.is_empty() => Some(true),
                KeyCode::Up if key.modifiers.is_empty() => Some(false),
                _ => None,
            };
            if let Some(down) = line {
                return Some(self.insert_cursor_move(Command::InsertStartColumnLine { down }));
            }
            return Some(match key.code {
                // `action` clears the transient axes (like every other completed key), so no partial
                // Normal state can survive an Insert key.
                KeyCode::Char('u') if !is_chord(key) => self.action(Command::BreakUndo),
                KeyCode::Char('U') if !is_chord(key) => {
                    self.reset();
                    self.insert.keep_undo = true;
                    Feed::Pending
                }
                _ => {
                    self.reset();
                    Feed::Ignored
                }
            });
        }
        // `i_CTRL-K` digraph prefix: once armed, the next TWO printable keys are the digraph code
        // (`:help i_CTRL-K`). Collected here, before the layer, so the code chars never reach text
        // insertion. A non-`Char` key (Esc, arrows, …) mid-sequence ABORTS cleanly — the pending state is
        // dropped and the key is ignored, staying in Insert (a minor divergence: Vim's mid-digraph <Esc>
        // also leaves Insert; here it only cancels the digraph). On the second char the pair is looked up;
        // an unknown pair falls back to inserting the SECOND char literally, matching Vim.
        if let Some(pending) = self.insert.digraph {
            // A chord is not a digraph char either: it aborts like a non-`Char` key.
            let (KeyCode::Char(c), false) = (key.code, is_chord(key)) else {
                self.insert.digraph = None;
                self.reset();
                return Some(Feed::Ignored);
            };
            match pending {
                DigraphPending::First => {
                    self.insert.digraph = Some(DigraphPending::Second(c));
                    return Some(Feed::Pending);
                }
                DigraphPending::Second(c1) => {
                    self.insert.digraph = None;
                    let ch = digraph(c1, c).unwrap_or(c);
                    return Some(self.action(Command::InsertChar(ch)));
                }
            }
        }
        // `i_CTRL-V` literal / numeric-code entry: once armed, drive the collector (`:help i_CTRL-V`).
        // Checked before the layer so the form selector / code digits / literal key never reach normal
        // text insertion. `take()` clears the state; the collector re-arms it while still gathering.
        if let Some(entry) = self.insert.literal.take() {
            return Some(self.feed_ctrl_v(entry, key));
        }
        None
    }

    /// A CONTROL chord in Insert that arms a prefix or runs an Insert command of its own. `last_typed` is
    /// the char typed just before (for `0 CTRL-D` / `^ CTRL-D`). `None` when the chord is not one of
    /// these, so it goes on to the CTRL aliases and the Insert layer.
    fn insert_chord(&mut self, key: KeyEvent, last_typed: Option<char>) -> Option<Feed> {
        Some(match key.code {
            // `i_CTRL-^` toggles the language map (Lang-Arg / lmap) on or off within Insert (F-027 / D-048).
            // MVP flips one boolean; the per-context iminsert/imsearch model is a follow-up. Checked before
            // the printable path so `^`/`6` under CTRL never reach text insertion.
            KeyCode::Char('^' | '6') => {
                self.reset();
                self.lang_active = !self.lang_active;
                Feed::Pending
            }
            KeyCode::Char('o') => {
                // Push a one-shot activation whose RETURN ADDRESS is Insert (KL-OBL-5): the NEXT keys resolve
                // through the Normal grammar; on completion `reset()` pops the address and Insert routing
                // resumes. Core mode stays Insert throughout. Reset first so the one-shot begins from a clean
                // count/operator/awaiting state (the pop is a no-op — the stack is empty here).
                self.reset();
                self.end_insert_change_for_one_shot();
                self.activations.push(Suspended { resume: Ns::Insert });
                Feed::Pending
            }
            KeyCode::Char('g') => {
                self.reset();
                self.insert.ctrl_g = true;
                Feed::Pending
            }
            // `i_CTRL-R` — arm the register-insert prefix; the next key names the register.
            KeyCode::Char('r') => {
                self.reset();
                self.insert.ctrl_r = true;
                Feed::Pending
            }
            // `i_CTRL-X` — arm the completion submode; the next key selects the source. Only `CTRL-L` (whole-line,
            // current buffer) is in scope this slice, resolved by the frontend (see the run loop). The Emacs
            // profile's `C-x` prefix is a SEPARATE non-modal path (`emacs_prefix`); this fires only in Vim/Native
            // Insert, which is the only profile set that reaches `feed_insert`.
            KeyCode::Char('x') => {
                self.reset();
                self.insert.ctrl_x = true;
                Feed::Pending
            }
            // `i_CTRL-K` — arm the digraph prefix; the next TWO printable keys select the digraph.
            KeyCode::Char('k') => {
                self.reset();
                self.insert.digraph = Some(DigraphPending::First);
                Feed::Pending
            }
            // `i_CTRL-V` — arm literal / numeric char entry; the next key(s) select the form and code
            // (Normal-mode `CTRL-V` is blockwise Visual — a separate path; this only fires in Insert). `i_CTRL-Q`
            // is the same key (`:help i_CTRL-Q`), for terminals that take `CTRL-V` for themselves.
            KeyCode::Char('v' | 'q') => {
                self.reset();
                self.insert.literal = Some(LiteralEntry::AwaitFirst);
                Feed::Pending
            }
            // `i_CTRL-W` / `i_CTRL-U` — delete the word before the caret / everything before it on the line.
            KeyCode::Char('w') => self.action(Command::InsertDeleteWordBack),
            KeyCode::Char('u') => self.action(Command::InsertDeleteToLineStart),
            // `i_CTRL-T` / `i_CTRL-D` — indent / dedent the current line by one shiftwidth.
            KeyCode::Char('t') => self.action(Command::InsertIndent),
            // `i_CTRL-F` — re-indent the line (`!^F` in 'indentkeys'); the frontend resolves it from the
            // buffer's indenter, and without one it does nothing (#1206).
            KeyCode::Char('f') => self.action(Command::InsertReindent),
            // `0 CTRL-D` / `^ CTRL-D` (the `0`/`^` typed just before): delete the typed char and ALL indent of the
            // line; after `^` the next `<CR>` restores it under 'autoindent' (#617). The two edits travel in a
            // `Replay`, which bypasses the recorder, so fold them in by hand.
            KeyCode::Char('d') if matches!(last_typed, Some('0' | '^')) => {
                self.reset();
                let delete = if last_typed == Some('^') {
                    Command::InsertDeleteIndentRestore
                } else {
                    Command::InsertDeleteIndent
                };
                let cmds = vec![Command::DeleteBack, delete];
                for cmd in &cmds {
                    self.record(&Feed::Cmd(cmd.clone()), Mode::Insert);
                }
                Feed::Replay(cmds)
            }
            KeyCode::Char('d') => self.action(Command::InsertDedent),
            // `i_CTRL-A` — re-insert the text of the MOST RECENT insert session at the caret (Vim's `".`
            // register). `i_CTRL-@` does the same and then leaves Insert (`:help i_CTRL-@`). Some terminals
            // deliver `CTRL-@` as `Ctrl+Space`; both are accepted here (Insert `Ctrl+Space` is otherwise
            // unbound in the Vim/Native profiles).
            KeyCode::Char('a') => self.insert_prev_text(false),
            KeyCode::Char('@' | ' ') => self.insert_prev_text(true),
            _ => return None,
        })
    }

    /// `i_CTRL-V` literal / numeric-code entry (`:help i_CTRL-V`). Drives the collector armed by
    /// `CTRL-V` in Insert. The first key picks a numeric form (decimal `{ddd}`, octal `o{ooo}`, hex
    /// `x{hh}`, BMP unicode `u{hhhh}`, full unicode `U{hhhhhhhh}`) or is inserted literally (a raw
    /// control key, `<Tab>`/`<CR>`/`<Esc>`, or any non-form char). Subsequent keys are the code digits.
    /// Collection resolves when the per-form digit cap is reached OR a non-matching key terminates it
    /// early — in which case the resolved char is emitted and the terminating key is processed as normal
    /// Insert input (re-dispatched here), matching Vim. Insert mode is implicit (the only caller).
    fn feed_ctrl_v(&mut self, entry: LiteralEntry, key: KeyEvent) -> Feed {
        match entry {
            LiteralEntry::AwaitFirst => self.ctrl_v_first(key),
            LiteralEntry::Collecting {
                base,
                value,
                remaining,
                count,
            } => self.ctrl_v_collect(base, value, remaining, count, key),
        }
    }

    /// The FIRST key after `CTRL-V`: arm a numeric form, or insert the key literally.
    fn ctrl_v_first(&mut self, key: KeyEvent) -> Feed {
        if !key.modifiers.contains(KeyModifiers::CONTROL) {
            if let KeyCode::Char(c) = key.code {
                if let Some(d) = c.to_digit(10) {
                    // Decimal: this char is the first of up to three digits.
                    self.insert.literal = Some(LiteralEntry::Collecting {
                        base: LiteralBase::Dec,
                        value: d,
                        remaining: 2,
                        count: 1,
                    });
                    return Feed::Pending;
                }
                // `o`/`O` octal (3), `x`/`X` hex byte (2), `u` BMP (4), `U` full Unicode (8).
                let form = match c {
                    'o' | 'O' => Some((LiteralBase::Oct, 3)),
                    'x' | 'X' => Some((LiteralBase::Hex, 2)),
                    'u' => Some((LiteralBase::Hex, 4)),
                    'U' => Some((LiteralBase::Hex, 8)),
                    _ => None,
                };
                if let Some((base, budget)) = form {
                    self.insert.literal = Some(LiteralEntry::Collecting {
                        base,
                        value: 0,
                        remaining: budget,
                        count: 0,
                    });
                    return Feed::Pending;
                }
            }
        }
        // Not a form starter: insert the key literally.
        self.insert_literal_key(key)
    }

    /// A key during numeric collection: extend the code if it is a valid digit for `base`, else terminate.
    fn ctrl_v_collect(
        &mut self,
        base: LiteralBase,
        value: u32,
        remaining: u8,
        count: u8,
        key: KeyEvent,
    ) -> Feed {
        let digit = match key.code {
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                c.to_digit(base.radix())
            }
            _ => None,
        };
        if let Some(d) = digit {
            let value = value * base.radix() + d;
            let remaining = remaining - 1;
            let count = count + 1;
            if remaining == 0 {
                // Digit cap reached: resolve now; there is no terminating key to re-process.
                return self.resolve_literal(base, value);
            }
            self.insert.literal = Some(LiteralEntry::Collecting {
                base,
                value,
                remaining,
                count,
            });
            return Feed::Pending;
        }
        // A non-digit terminates collection.
        if count == 0 {
            // No digits were collected (only reachable for the o/x/u/U prefixes): the terminating key is
            // inserted literally and no code char is produced (Vim: `CTRL-V x z` -> `z`, `CTRL-V o 8` -> `8`).
            return self.insert_literal_key(key);
        }
        // >=1 digit: resolve the code, then process the terminating key as normal Insert input.
        let Some(c) = self.resolve_char(base, value) else {
            // Invalid code point (a lone surrogate via `u`/`U`, say): nothing to prepend — just the term.
            return self.feed_impl(key, Mode::Insert);
        };
        // Emit the resolved char AHEAD of whatever the terminator produces. `Replay` is the only multi-command
        // channel and it bypasses the recorder, so fold the char into the dot-repeat record by hand FIRST —
        // before the terminator runs, since a terminator like `<C-a>` records its own commands (#797).
        self.record(&Feed::Cmd(Command::InsertChar(c)), Mode::Insert);
        match self.feed_impl(key, Mode::Insert) {
            Feed::Cmd(tc) => {
                self.record(&Feed::Cmd(tc.clone()), Mode::Insert);
                Feed::Replay(vec![Command::InsertChar(c), tc])
            }
            // A terminator that is itself a multi-command replay (`<C-a>` / `<C-@>`), already recorded.
            Feed::Replay(tcmds) => {
                let mut cmds = vec![Command::InsertChar(c)];
                cmds.extend(tcmds);
                Feed::Replay(cmds)
            }
            // The terminator armed a prefix / was ignored: only the resolved char applies now. Any Insert
            // prefix it just armed survives (the reset clears only Normal state).
            _ => {
                self.reset();
                Feed::Replay(vec![Command::InsertChar(c)])
            }
        }
    }

    /// Insert `key` literally (`CTRL-V`'s literal path): a raw control key becomes its control byte,
    /// `<Tab>`/`<CR>`/`<Esc>` their code points, any other char itself. An exotic key (arrows, F-keys)
    /// aborts cleanly, staying in Insert.
    fn insert_literal_key(&mut self, key: KeyEvent) -> Feed {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if let KeyCode::Char(c) = key.code {
                if c.is_ascii() {
                    // `CTRL-<c>` -> its control byte (`CTRL-V` -> 0x16, `CTRL-A` -> 0x01, ...).
                    return self.action(Command::InsertChar(((c as u8) & 0x1f) as char));
                }
            }
            self.reset();
            return Feed::Ignored;
        }
        match key.code {
            KeyCode::Char(c) => self.action(Command::InsertChar(c)),
            KeyCode::Tab => self.action(Command::InsertChar('\t')),
            KeyCode::Enter => self.action(Command::InsertChar('\r')),
            KeyCode::Esc => self.action(Command::InsertChar('\u{1b}')),
            _ => {
                self.reset();
                Feed::Ignored
            }
        }
    }

    /// Resolve an accumulated numeric code to its `InsertChar` command (the cap-reached path).
    fn resolve_literal(&mut self, base: LiteralBase, value: u32) -> Feed {
        match self.resolve_char(base, value) {
            Some(c) => self.action(Command::InsertChar(c)),
            None => {
                self.reset();
                Feed::Ignored
            }
        }
    }

    /// Map an accumulated numeric code to a `char`. Decimal/octal clamp to a single byte (255, Vim's cap);
    /// hex/unicode are validated by `char::from_u32` — a lone surrogate or an out-of-range code point
    /// yields `None`, which the caller drops. (A documented divergence: Vim stores such raw bytes as
    /// invalid UTF-8, which ruse's UTF-8 text buffer cannot represent via `InsertChar`. Byte values
    /// 0-255 map to U+0000..=U+00FF and are inserted as valid UTF-8, matching Neovim's `C-v 200` -> È.)
    fn resolve_char(&self, base: LiteralBase, value: u32) -> Option<char> {
        let v = match base {
            LiteralBase::Dec | LiteralBase::Oct => value.min(255),
            LiteralBase::Hex => value,
        };
        char::from_u32(v)
    }

    /// Replace (`R`) / Virtual Replace (`gR`): the `open/overwrite` namespace. Bindings (Esc/BS/CR)
    /// resolve through the Replace LAYER; an unmatched printable key hits the layer's declared overwrite
    /// policy (KL-OBL-2), applied as the mode-appropriate overwrite command (tab-aware in Virtual Replace).
    pub(super) fn feed_replace(&mut self, key: KeyEvent, mode: Mode) -> Feed {
        // The Insert-family CTRL aliases (see `feed_insert`): `CTRL-C`/`<C-[>` = `<Esc>`, `CTRL-H` = `<BS>`,
        // `CTRL-J`/`<C-m>` = `<CR>`. Any other chord is unbound and overwrites nothing (#599).
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let key = match key.code {
            KeyCode::Char('c' | '[') if ctrl => plain(KeyCode::Esc),
            KeyCode::Char('h') if ctrl => plain(KeyCode::Backspace),
            KeyCode::Char('j' | 'm') if ctrl => plain(KeyCode::Enter),
            _ => key,
        };
        // `<Insert>` in Replace toggles to Insert (`:help i_<Insert>`).
        if key.code == KeyCode::Insert && key.modifiers.is_empty() && mode == Mode::Replace {
            return self.action(Command::EnterInsert);
        }
        if let Resolved::Bound { value, .. } = self.profile.stack(Ns::Replace).resolve(&key.code) {
            let cmd = value.clone();
            return self.action(cmd);
        }
        // open/overwrite: a printable key overwrites; non-printable does nothing (NOT closed/ignore).
        debug_assert!(
            matches!(
                self.profile.stack(Ns::Replace).resolve(&key.code),
                Resolved::Unmatched {
                    policy: UnmatchedKey::Overwrite,
                    ..
                }
            ),
            "the Replace namespace must declare open/overwrite"
        );
        self.reset();
        match key.code {
            KeyCode::Char(_) if is_chord(key) => Feed::Ignored,
            KeyCode::Char(c) if mode == Mode::VirtualReplace => {
                Feed::Cmd(Command::VirtualReplaceType(c))
            }
            KeyCode::Char(c) => Feed::Cmd(Command::ReplaceType(c)),
            _ => Feed::Ignored,
        }
    }
}

/// The command an unmodified Insert-mode cursor key moves by. None of them leaves the line sideways:
/// `<Left>` stops at the first char and `<Right>` after the last ('whichwrap' has no `[`/`]` by default);
/// `<End>` rests after the last char and keeps that column wanted for `<Up>`/`<Down>`.
fn insert_cursor_command(code: KeyCode) -> Option<Command> {
    Some(match code {
        KeyCode::Left => Command::Move(1, Motion::Left),
        KeyCode::Right => Command::Move(1, Motion::Right),
        KeyCode::Up => Command::Move(1, Motion::Up),
        KeyCode::Down => Command::Move(1, Motion::Down),
        KeyCode::Home => Command::MoveLineStart,
        KeyCode::End => Command::MoveLineEnd,
        _ => return None,
    })
}
