//! The Normal-family key paths (#1356): `feed_impl` (layer teardown and the awaited-key tier),
//! `feed_base` (operator-pending guard, shared initiators, Visual/Select keys, the count/operator/motion
//! grammar) and `feed_chord`. Moved out of `input/mod.rs` as an `impl InputEngine` block.
//!
//! Children: `awaiting` (the decoder for each awaited key), `base` (the count/operator/motion
//! grammar), `g_prefix` (the `g{char}` second key) and `selection` (the Visual/Select keys).

use super::*;

mod awaiting;
mod base;
mod g_prefix;
mod selection;

impl InputEngine {
    /// A CONTROL/ALT chord in the Normal family (Normal, Visual, Select, operator-pending, an `i_CTRL-O`
    /// one-shot), reached only with `awaiting == Nothing`. Only the chords below are bound; every other one
    /// takes its namespace's unmatched policy and never decodes as the bare letter (#598, #1059). Ground
    /// truth is nvim v0.12.6 (the table in `input/chord_tests.rs` pins it). `<C-c>` is handled
    /// in `feed_impl`; the chords the frontend owns in Normal (`<C-w>`, `<C-d>`/`<C-u>`, `<C-e>`/`<C-y>`,
    /// `<C-b>`/`<C-f>`/`<C-l>`/`<C-p>`, `<C-]>`) are taken before the engine sees them.
    fn feed_chord(&mut self, key: KeyEvent, mode: Mode) -> Feed {
        let selection = match mode {
            Mode::Visual { kind } | Mode::Select { kind } => Some(kind),
            _ => None,
        };
        let select = matches!(mode, Mode::Select { .. });
        let op = self.normal.op.is_some();
        let only_ctrl = key.modifiers - KeyModifiers::SHIFT == KeyModifiers::CONTROL;
        let c = match key.code {
            KeyCode::Char(c) if only_ctrl => c,
            _ => '\0', // an ALT/SUPER/… chord: nothing is bound
        };
        match c {
            // `CTRL-G` toggles Visual<->Select over the SAME selection; in Normal it is file info (name /
            // [Modified] / line count / cursor percent), resolved + status-surfaced by the frontend.
            'g' if !op => match mode {
                Mode::Visual { kind } => self.action(Command::EnterSelect { kind }),
                Mode::Select { kind } => self.action(Command::EnterVisual { kind }),
                Mode::Normal => self.action(Command::FileInfo),
                _ => self.unmatched(Ns::Normal, key),
            },
            // `<C-[>` is `<Esc>`, `<C-m>` is `<CR>` (the kitty protocol sends them as chords).
            '[' => self.feed_base(plain(KeyCode::Esc), mode),
            // `CTRL-\` (a legacy terminal sends it as `Ctrl-4`) arms `CTRL-\ CTRL-N` / `CTRL-\ CTRL-G`.
            '\\' | '4' => self.arm_awaiting(Awaiting::CtrlBackslash),
            'm' => self.feed_base(plain(KeyCode::Enter), mode),
            // Motions (`:help CTRL-H`, `CTRL-J`, `CTRL-N`, `CTRL-P`): `<C-h>` = `h`, `<C-j>`/`<C-n>` = `j`,
            // `<C-p>` = `k`, with a count and under an operator (`d<C-j>` = `dj`). In Select, `CTRL-H` deletes
            // the selection like `<BS>` and `CTRL-J` (`<NL>`) replaces it like `<CR>`; `CTRL-N`/`CTRL-P` still
            // extend it (non-printable movement, `:help Select-mode`).
            'h' if select => self.action(Command::DeleteSelection),
            'j' if select => self.action(Command::ReplaceSelection('\n')),
            'h' => self.motion(Motion::Left),
            'j' | 'n' => self.motion(Motion::Down),
            'p' => self.motion(Motion::Up),
            // `v_CTRL-O` in Select: run ONE Visual command, then return to Select (see `feed`).
            'o' if select => {
                let kind = selection.expect("Select is a selection mode");
                let out = self.action(Command::EnterVisual { kind });
                self.select_shot = true;
                out
            }
            // `CTRL-V` (and its alias `CTRL-Q`): blockwise Visual; in a selection it switches the shape (the
            // same key again leaves it); under an operator it forces the motion blockwise (`d<C-v>j`).
            'v' | 'q' => match selection {
                Some(SelectKind::Blockwise) => self.action(Command::EnterNormal),
                Some(_) => self.action(Command::EnterVisual {
                    kind: SelectKind::Blockwise,
                }),
                None if op => {
                    self.normal.forced_wise = Some(ForcedWise::Blockwise);
                    Feed::Pending
                }
                None => self.action(Command::EnterVisual {
                    kind: SelectKind::Blockwise,
                }),
            },
            // `CTRL-A` / `CTRL-X` — add ±count to the number under-or-after the cursor, or to the first
            // number on every selected line.
            'a' | 'x' if !op => {
                let delta = i64::from(self.mcount());
                let delta = if c == 'a' { delta } else { -delta };
                if selection.is_some() {
                    self.action(Command::IncrementSelection {
                        delta,
                        sequential: false,
                    })
                } else {
                    self.action(Command::IncrementNumber(delta))
                }
            }
            // `CTRL-O` / `CTRL-I` walk the jumplist back / forward; `CTRL-R` redoes. Normal only.
            'o' if !op && selection.is_none() => self.action(Command::GotoOlderJump),
            'i' if !op && selection.is_none() => self.action(Command::GotoNewerJump),
            'r' if !op && selection.is_none() => self.counted(Command::Redo),
            _ => {
                let ns = if op {
                    Ns::OperatorPending
                } else if select {
                    Ns::Select
                } else if selection.is_some() {
                    Ns::Visual
                } else {
                    Ns::Normal
                };
                self.unmatched(ns, key)
            }
        }
    }

    /// The base Normal-family dispatch, reached only with `awaiting == Nothing` (the tier above
    /// returned for the pending cases). Shared char-search initiators, then the count/operator/motion
    /// grammar and the mode-specific keys.
    fn feed_base(&mut self, key: KeyEvent, mode: Mode) -> Feed {
        // Operator-pending (Vim's `checkclearop`): a key that cannot continue the armed operator ABORTS the
        // whole construct and is consumed — `ydw` must not delete, `yx` must not cut, `dJ` must not join
        // (#790). Checked first, so no Normal-only arm below can run with an operator armed.
        // A chord is exempt: `feed_chord` owns the chord table and its own operator-pending rule (`d<C-j>`
        // continues, `d<C-k>` aborts), and `<C-Left>`/`<S-Right>` are word motions.
        if let Some(p) = self.normal.op {
            // `{count}<Del>` edits the count being typed (`d23<Del>w` = `d2w`), so it continues too.
            let count_edit = key.code == KeyCode::Delete && self.normal.count > 0;
            // `dp`/`do` are `:diffput`/`:diffget`; ruse has no diff mode, so they fail as nvim does outside
            // one (#1087).
            if p.op == Op::Delete
                && key.modifiers.is_empty()
                && matches!(key.code, KeyCode::Char('p' | 'o'))
            {
                self.error = Some("E99: Current buffer is not in diff mode");
            }
            if !is_chord(key) && !count_edit && !continues_op(p.op, key) {
                return self.unmatched(Ns::OperatorPending, key);
            }
        }
        // A chord resolves in its own table and never reaches the bare-key arms below (#598, #1059).
        if is_chord(key) && matches!(key.code, KeyCode::Char(_)) {
            return self.feed_chord(key, mode);
        }
        // `<S-Left>`/`<C-Left>` and `<S-Right>`/`<C-Right>` move by word (`b`/`w`), not by char. `<C-Home>` is
        // `gg`; `<C-End>` goes to line `{count}` (default the last) on its LAST char (nvim v0.12.6). The other
        // modified special keys keep their plain meaning (`<S-Up>`/`<S-Down>` page-scroll in the frontend).
        if !key.modifiers.is_empty() {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Left => return self.motion(Motion::WordBack),
                KeyCode::Right => return self.motion(Motion::WordFwd),
                KeyCode::Home if ctrl => return self.motion(Motion::GotoLine),
                KeyCode::End if ctrl => {
                    let line = if self.normal.typed() > 0 {
                        Motion::GotoLine
                    } else {
                        Motion::LastLine
                    };
                    // Under an operator it is linewise like `G` (nvim's is charwise-inclusive to the last
                    // char; the two agree whenever the start is at or before the first non-blank).
                    if self.normal.op.is_some() {
                        return self.motion(line);
                    }
                    let n = self.mcount();
                    self.reset();
                    return Feed::Replay(vec![
                        Command::Move(n, line),
                        Command::Move(1, Motion::LineEnd),
                    ]);
                }
                _ => {}
            }
        }
        // Select (`:help Select-mode`): EVERY printable key — letters, digits, `"`, `:`, space, all the keys
        // Visual binds — replaces the selection and enters Insert, so it must win before the shared
        // initiators below (#792). `<CR>` replaces it with a line break, `<BS>`/`<Del>` delete it. The
        // non-printable keys (arrows, `<Home>`/`<End>`, `<Esc>`) fall through to Visual's handling, so
        // movement extends the selection.
        if let Mode::Select { .. } = mode {
            match key.code {
                KeyCode::Char(c) => return self.action(Command::ReplaceSelection(c)),
                KeyCode::Enter => return self.action(Command::ReplaceSelection('\n')),
                KeyCode::Backspace | KeyCode::Delete => {
                    return self.action(Command::DeleteSelection)
                }
                _ => {}
            }
        }
        if let Some(feed) = self.base_initiator(key) {
            return feed;
        }
        // Visual and Select: the selection already exists, so operators act on it directly and motions
        // extend it. The two share every matched key here (identical selection state); they diverge ONLY
        // in the unmatched-key fallthrough — Visual ignores, Select replaces-and-inserts.
        //
        // `gv` (restore the previous selection) IS wired — it re-selects the depth-1 `last_visual` (handled
        // in the `g`-initiator tier above); the full C-ANCHOR position history stays deferred.
        if let Mode::Visual { kind } | Mode::Select { kind } = mode {
            if let Some(feed) = self.base_selection(key, mode, kind) {
                return feed;
            }
        }
        self.base_grammar(key)
    }

    /// The shared initiators (char-search + `;`/`,`): work in Normal and Visual, preserving the operator
    /// axis (so `dfx` / `d;` work). Reachable only with `awaiting == Nothing` — the tier above already
    /// returned for the pending cases — so a text object in flight can never be hijacked by `f`/`t`. Most
    /// arm a one-key expectation; `None` when the key is not an initiator.
    fn base_initiator(&mut self, key: KeyEvent) -> Option<Feed> {
        let awaiting = match key.code {
            // `f` / `F` / `t` / `T` — the next key is the target char.
            KeyCode::Char(c @ ('f' | 'F' | 't' | 'T')) => Awaiting::FindTarget {
                forward: c.is_ascii_lowercase(),
                till: matches!(c, 't' | 'T'),
            },
            KeyCode::Char(c @ (';' | ',')) => {
                let (ch, forward, till) = self.last_find?;
                // A repeat: after `t`/`T` the core skips the char the cursor stopped before (#802). `,`
                // repeats in the opposite direction.
                return Some(self.motion(Motion::FindChar {
                    ch,
                    forward: if c == ';' { forward } else { !forward },
                    till,
                    repeat: true,
                }));
            }
            // Line jumps: `g` arms `gg`; `G` jumps to `{count}` (or the last line when no count). In Visual
            // too (Select returned above), so `gu`/`gU`/`g~` recase the selection and `gg`/`ge`/`g_` extend it.
            KeyCode::Char('g') => Awaiting::GSecond,
            // The count may have been typed BEFORE the operator (`2dG` = `d2G`, #791): read every segment.
            KeyCode::Char('G') => {
                return Some(if self.normal.typed() > 0 {
                    self.motion(Motion::GotoLine)
                } else {
                    self.motion(Motion::LastLine)
                });
            }
            // Bare `%` matches the bracket; `{count}%` is Vim's percentage jump (go to count% of the file).
            KeyCode::Char('%') => {
                return Some(if self.normal.typed() > 0 {
                    self.motion(Motion::GotoPercent)
                } else {
                    self.motion(Motion::MatchBracket)
                });
            }
            // `]` / `[` — arm a bracket-command prefix; the next key selects the command. Only the indent-
            // adjusting pastes `]p`/`]P`/`[p`/`[P` are wired. The count (`3]p`) is preserved for it.
            KeyCode::Char(c @ (']' | '[')) => Awaiting::BracketPrefix {
                open_bracket: c == '[',
            },
            // `"x` — arm register selection. Deliberately does NOT reset the count/operator axes, so a
            // count typed after it still lands (`"a3yy`). The next key is the register name (see the
            // `RegisterSelect` tier above). Shared by Normal and Visual (Vim supports `"ayiw` and `"xy`).
            KeyCode::Char('"') => Awaiting::RegisterSelect,
            // `` ` `` — arm a mark jump; the next key names the mark (`.` = last change, or `a`–`z`).
            KeyCode::Char('`') => Awaiting::MarkJump { keep_jumps: false },
            // `m` — arm a mark SET; the next key names the mark (`a`–`z`).
            KeyCode::Char('m') => Awaiting::SetMarkChar,
            // `'` — arm a LINEWISE mark jump; the next key names the mark (`.` or `a`–`z`).
            KeyCode::Char('\'') => Awaiting::MarkJumpLine { keep_jumps: false },
            _ => return None,
        };
        Some(self.arm_awaiting(awaiting))
    }

    pub(super) fn feed_impl(&mut self, key: KeyEvent, mode: Mode) -> Feed {
        // KL-OBL-4: a layer's owned state is destroyed when the layer deactivates — the engine does not
        // reach in to reset foreign fields.
        //
        // The Insert layer owns the `CTRL-G` prefix; it dies the moment the active namespace is not
        // Insert (a key in any other mode means the insert context is gone). A pending Insert one-shot
        // return (KL-OBL-5) is likewise abandoned — its resume address no longer applies.
        if mode != Mode::Insert {
            self.insert = InsertState::default();
            self.activations.retain(|a| a.resume != Ns::Insert);
        }
        // The Normal-family grammar layer (Normal / Visual / Select, with operator-pending as its
        // sub-state) owns count / operator / awaiting / forced-wise. They die when neither the family
        // nor an `i_CTRL-O` one-shot — which runs a single Normal command from WITHIN Insert, so the
        // family is momentarily active there — is in effect.
        let normal_family = matches!(
            mode,
            Mode::Normal | Mode::Visual { .. } | Mode::Select { .. }
        );
        if !normal_family && !self.in_one_shot() {
            self.normal = NormalState::default();
        }
        // Insert resolves through its LAYER, not through an early return ahead of everything else.
        // The bindings and the `open/insert` policy both live in `VimProfile`, so the namespace is
        // addressable in its own right (KL-OBL-1) and its policy is declared (KL-OBL-2).
        //
        // Two multi-key insert sequences are handled BEFORE the layer: `CTRL-O` (push a one-shot Normal
        // activation, then fall through to the Normal grammar for the rest of this and following keys until
        // it completes) and `CTRL-G u` (undo-break). A `CTRL-O` already in flight (on the activation stack)
        // skips the insert branch entirely so the pending Normal command keeps resolving.
        if mode == Mode::Insert && !self.in_one_shot() {
            return self.feed_insert(key);
        }
        if mode == Mode::Replace || mode == Mode::VirtualReplace {
            return self.feed_replace(key, mode);
        }
        // --- Native leader tier (F-013 NAT-2), ABOVE the Vim grammar and gated to the Native profile. ---
        // An ARMED leader consumes this key as its which-key selection (a bound command, or a which-key
        // abort). `self.leader` is only ever set on the Native+Normal path, so this is inert elsewhere.
        if self.leader {
            self.leader = false;
            return native_leader_command(key).map_or(Feed::Ignored, Feed::Cmd);
        }
        // ARM the leader from a CLEAN Normal base: `<leader>` (Space) opens the menu. Gated to Native +
        // Normal + clean so Vim's Space=MoveRight and a mid-construct Space-as-motion stay intact (NAT-1).
        if self.input_profile == InputProfile::Native
            && mode == Mode::Normal
            && self.normal.is_clean()
            && key.code == KeyCode::Char(' ')
            && key.modifiers.is_empty()
        {
            self.leader = true;
            return Feed::Pending;
        }
        // `CTRL-C` is Vim's interrupt (`:help CTRL-C`): it cancels whatever is pending (count, operator,
        // awaited key) and leaves Visual / Select; in a clean Normal it does nothing. It must never reach the
        // bare-key arms, where it decoded as the `c` operator, so `<C-c><C-c>` was `cc` (#897).
        if normal_family
            && !self.in_one_shot()
            && key.code == KeyCode::Char('c')
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            let had_register = self.normal.register;
            self.reset();
            return if mode == Mode::Normal {
                self.drop_aborted_register(had_register, Feed::Ignored)
            } else {
                self.action(Command::EnterNormal)
            };
        }
        // A chord is never the char an awaited key names (`f<C-a>` is not `fa`, `"<C-a>` is not register
        // `a`): it aborts the pending construct, as in nvim. Only the `g` and `CTRL-\` prefixes bind chords.
        if is_chord(key)
            && matches!(key.code, KeyCode::Char(_))
            && !matches!(
                self.normal.awaiting,
                Awaiting::Nothing | Awaiting::GSecond | Awaiting::CtrlBackslash
            )
        {
            self.normal.awaiting = Awaiting::Nothing;
            return self.unmatched(Ns::OperatorPending, key);
        }
        // --- Top-priority tier: a one-shot key-expectation resolves before any base-key handling. ---
        match self.feed_awaiting(key, mode) {
            Some(feed) => feed,
            None => self.feed_base(key, mode),
        }
    }

    /// Arm a one-key expectation: the NEXT key resolves through [`feed_awaiting`](Self::feed_awaiting).
    /// The count, operator and register stay armed for it.
    fn arm_awaiting(&mut self, awaiting: Awaiting) -> Feed {
        self.normal.awaiting = awaiting;
        Feed::Pending
    }

    /// The top-priority tier: a one-shot key expectation (`Awaiting`) resolves before any base-key
    /// handling, and is cleared first. `None` when nothing is awaited.
    fn feed_awaiting(&mut self, key: KeyEvent, mode: Mode) -> Option<Feed> {
        Some(match std::mem::take(&mut self.normal.awaiting) {
            Awaiting::Nothing => return None,
            Awaiting::FindTarget { forward, till } => self.await_find_target(key, forward, till),
            Awaiting::TextObjectChar { inner } => self.await_text_object(key, inner),
            Awaiting::GSecond => self.await_g_second(key, mode),
            Awaiting::ReplaceChar => self.await_replace_char(key, mode),
            Awaiting::VirtualReplaceChar => self.await_virtual_replace_char(key),
            // `ZZ` / `ZQ` are exactly `:x` / `:q!` (`:help ZZ`), so they run the same Ex quit policy: one
            // place decides window-close versus editor exit and the unsaved-changes refusals (E37/E162).
            // `CTRL-\ CTRL-N` / `CTRL-\ CTRL-G` go to Normal mode: they leave a selection and drop a count or
            // operator, and do nothing in Normal (nvim v0.12.6). Any other key aborts.
            Awaiting::CtrlBackslash => self.await_ctrl_backslash(key, mode),
            Awaiting::ZSecond => self.await_z_second(key),
            Awaiting::MarkJump { keep_jumps } => self.await_mark_jump(key, keep_jumps),
            Awaiting::SetMarkChar => self.await_set_mark(key),
            Awaiting::MarkJumpLine { keep_jumps } => self.await_mark_jump_line(key, keep_jumps),
            Awaiting::RegisterSelect => self.await_register(key),
            Awaiting::BracketPrefix { open_bracket } => self.await_bracket(key, open_bracket),
        })
    }
}
