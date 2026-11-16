//! The Normal-family key paths (#1356): `feed_impl` (layer teardown and the awaited-key tier),
//! `feed_base` (operator-pending guard, shared initiators, Visual/Select keys, the count/operator/motion
//! grammar) and `feed_chord`. Moved out of `input/mod.rs` as an `impl InputEngine` block.

use super::*;

mod g_prefix;

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

    /// Visual and Select keys over an existing selection; `None` falls through to the shared count /
    /// motion grammar (no operator is ever armed in a selection).
    fn base_selection(&mut self, key: KeyEvent, mode: Mode, kind: SelectKind) -> Option<Feed> {
        // `v`/`V`/`CTRL-V` (`feed_chord`) switch the selection SHAPE: pressing the key of the current shape leaves the
        // namespace (to Normal), any other switches to that shape (F-025 c1). `i`/`a` (lowercase) begin
        // a text object in every shape; blockwise-only `I`/`A` (and block `c`/`s`) arm an insert-
        // replicate session instead of the plain charwise change.
        let is_block = kind == SelectKind::Blockwise;
        let shape_toggle = |target: SelectKind| {
            if kind == target {
                Command::EnterNormal
            } else {
                Command::EnterVisual { kind: target }
            }
        };
        match key.code {
            KeyCode::Esc => return Some(self.action(Command::EnterNormal)),
            KeyCode::Char('v') => return Some(self.action(shape_toggle(SelectKind::Charwise))),
            KeyCode::Char('V') => return Some(self.action(shape_toggle(SelectKind::Linewise))),
            // Blockwise insert-replicate: `I` at the left edge, `A` at the right edge, `c`/`s` delete
            // the block then insert at the left edge — each replicates on `<Esc>` (blockwise slice 2).
            KeyCode::Char('I') if is_block => {
                return Some(self.action(Command::BlockInsert(BlockInsertKind::Insert)))
            }
            KeyCode::Char('A') if is_block => {
                return Some(self.action(Command::BlockInsert(BlockInsertKind::Append)))
            }
            KeyCode::Char('c') | KeyCode::Char('s') if is_block => {
                return Some(self.action(Command::BlockInsert(BlockInsertKind::Change)))
            }
            KeyCode::Char('d') | KeyCode::Char('x') => {
                return Some(self.action(Command::DeleteSelection))
            }
            KeyCode::Char('y') => return Some(self.action(Command::YankSelection)),
            KeyCode::Char('c') | KeyCode::Char('s') => {
                return Some(self.action(Command::ChangeSelection))
            }
            // Visual `u`/`U`/`~` recase the selection (Visual only; in Select a printable key replaces
            // it via the namespace policy). `gu`/`gU`/`g~` in Visual are a follow-up.
            KeyCode::Char('u') if matches!(mode, Mode::Visual { .. }) => {
                return Some(self.action(Command::CaseSelection(WordCase::Downcase)))
            }
            KeyCode::Char('U') if matches!(mode, Mode::Visual { .. }) => {
                return Some(self.action(Command::CaseSelection(WordCase::Upcase)))
            }
            KeyCode::Char('~') if matches!(mode, Mode::Visual { .. }) => {
                return Some(self.action(Command::CaseSelection(WordCase::Toggle)))
            }
            // Visual `r{char}` — replace every selected char with the next key (Vim `v_r`). Arm the
            // shared ReplaceChar expectation; its resolution sees the selection mode and emits the
            // selection form. Keeps the count axis (unused here) like Normal `r`.
            KeyCode::Char('r') => {
                return Some(self.arm_awaiting(Awaiting::ReplaceChar));
            }
            // Visual `p`/`P` — replace the selection with the register. `p` swaps the deleted text into
            // the unnamed register; `P` preserves it (paste the same thing over successive selections).
            KeyCode::Char(c @ ('p' | 'P')) => {
                let count = self.mcount();
                return Some(self.action(Command::PasteSelection {
                    swap: c == 'p',
                    count,
                }));
            }
            // `o` swaps the selection's ends (cursor <-> anchor); the SAME text stays selected but a
            // later motion extends the other end. In Normal `o` is OpenBelow — here it is the swap.
            KeyCode::Char('o') => return Some(self.action(Command::SwapSelectionEnds)),
            // `O`: in a block, the other corner on the same row; otherwise the same as `o`.
            KeyCode::Char('O') if is_block => {
                return Some(self.action(Command::VisualOp(VisualOp::OtherCorner)))
            }
            KeyCode::Char('O') => return Some(self.action(Command::SwapSelectionEnds)),
            // `/` and `?` open the search line; the match it lands on extends the selection, as any
            // motion does (`v_/`, #1113). In Select a printable key replaces the selection instead.
            KeyCode::Char(c @ ('/' | '?')) if matches!(mode, Mode::Visual { .. }) => {
                return Some(self.enter_search(c == '?'))
            }
            // `n` / `N` repeat the last search as a motion that extends the selection.
            KeyCode::Char('n' | 'N') if matches!(mode, Mode::Visual { .. }) => {}
            // `:` leaves Visual (setting `'<`/`'>`) and opens the command line on `'<,'>`; `!` opens it
            // on `'<,'>!`, the filter over the selected lines (`:help v_:`, `v_!`).
            KeyCode::Char(c @ (':' | '!')) => {
                let out = self.action(Command::VisualOp(VisualOp::LeaveAtStart));
                self.open_filter_cmdline(if c == ':' { "'<,'>" } else { "'<,'>!" });
                return Some(out);
            }
            // Line operators over the selection (`:help v_>`, `v_J`, `v_=`): `{count}>` shifts `count`
            // levels. The uppercase forms work on whole lines; in a block `D`/`C` run to each line's end
            // and `X`/`Y` are the block `d`/`y` (nvim v0.12.6).
            KeyCode::Char(c @ ('>' | '<')) => {
                return Some(self.action(Command::VisualOp(VisualOp::Shift {
                    left: c == '<',
                    count: self.mcount(),
                })))
            }
            KeyCode::Char('=') => return Some(self.action(Command::VisualOp(VisualOp::Reindent))),
            KeyCode::Char('J') => {
                return Some(self.action(Command::VisualOp(VisualOp::Join { spaces: true })))
            }
            KeyCode::Char(c @ ('D' | 'C')) if is_block => {
                let op = if c == 'D' {
                    OpKind::Delete
                } else {
                    OpKind::Change
                };
                return Some(self.action(Command::VisualOp(VisualOp::BlockToEol(op))));
            }
            KeyCode::Char('X') if is_block => return Some(self.action(Command::DeleteSelection)),
            KeyCode::Char('Y') if is_block => return Some(self.action(Command::YankSelection)),
            KeyCode::Char(c @ ('D' | 'X' | 'Y' | 'C' | 'S' | 'R')) => {
                let op = match c {
                    'D' | 'X' => OpKind::Delete,
                    'Y' => OpKind::Yank,
                    _ => OpKind::Change,
                };
                return Some(self.action(Command::VisualOp(VisualOp::Lines(op))));
            }
            // In a selection, `i`/`a` always begin a text object (there is no insert here); the next key
            // is its selector. The completed object re-spans the selection (see the core's `Move` arm).
            KeyCode::Char('i') => {
                return Some(self.arm_awaiting(Awaiting::TextObjectChar { inner: true }));
            }
            KeyCode::Char('a') => {
                return Some(self.arm_awaiting(Awaiting::TextObjectChar { inner: false }));
            }
            // Count digits and motions extend the selection; an unmatched key hits the namespace's
            // own policy — `closed/ignore` for Visual, `open/replace-selection` for Select.
            KeyCode::Char('1'..='9') => {}
            KeyCode::Char('0') if self.normal.count > 0 => {}
            KeyCode::Char('0') => return Some(self.motion(Motion::LineStart)),
            _ if motion_key(key.code).is_some() => {}
            // `<Space>`/`<BS>` extend a Visual selection like `l`/`h`, wrapping across lines. In Select
            // they stay with the namespace policy (a typed space replaces the selection).
            KeyCode::Char(' ') | KeyCode::Backspace
                if matches!(mode, Mode::Visual { .. }) && key.modifiers.is_empty() => {}
            _ => {
                let ns = if matches!(mode, Mode::Select { .. }) {
                    Ns::Select
                } else {
                    Ns::Visual
                };
                return Some(self.unmatched(ns, key));
            }
        }
        // fall through to shared count/motion handling below (op is never set in Visual/Select)
        None
    }

    /// The count / operator / motion grammar and the Normal-only keys: what is left of `feed_base` once
    /// the guards, the initiators and the selection keys passed the key on.
    fn base_grammar(&mut self, key: KeyEvent) -> Feed {
        match key.code {
            KeyCode::Char(d @ '1'..='9') => {
                self.normal.count = push_count_digit(self.normal.count, d as u32 - '0' as u32);
                Feed::Pending
            }
            KeyCode::Char('0') if self.normal.count > 0 => {
                self.normal.count = push_count_digit(self.normal.count, 0);
                Feed::Pending
            }
            KeyCode::Char('0') => self.motion(Motion::LineStart),
            code if motion_key(code).is_some() => {
                self.motion(motion_key(code).expect("guarded by is_some"))
            }
            // With an operator armed, `v`/`V` (and `CTRL-V`, in `feed_chord`) FORCE the next motion's wise
            // (Vim `o_v`/`o_V`/`o_CTRL-V`): `dvj`, `dVe`, `d<C-v>j`. They stay operator-pending (the motion
            // still follows); `motion` emits `OpForced`. Bare (no operator) they enter Visual/Visual-line.
            KeyCode::Char('v') if self.normal.op.is_some() => {
                self.normal.forced_wise = Some(ForcedWise::Charwise);
                Feed::Pending
            }
            KeyCode::Char('V') if self.normal.op.is_some() => {
                self.normal.forced_wise = Some(ForcedWise::Linewise);
                Feed::Pending
            }
            KeyCode::Char('v') => self.action(Command::EnterVisual {
                kind: SelectKind::Charwise,
            }),
            KeyCode::Char('V') => self.action(Command::EnterVisual {
                kind: SelectKind::Linewise,
            }),
            KeyCode::Char('d') => self.operator(Op::Delete),
            // `gcc` — the doubled comment operator: its `c` is the doubling key, not `c` (#618).
            KeyCode::Char('c')
                if matches!(
                    self.normal.op,
                    Some(OpPending {
                        op: Op::Comment,
                        ..
                    })
                ) =>
            {
                self.op_linewise()
            }
            KeyCode::Char('c') => self.operator(Op::Change),
            KeyCode::Char('y') => self.operator(Op::Yank),
            // `>`/`<` are operators: doubled (`>>`) shifts the current `count` lines, and over a motion
            // (`>j`, `>ap`) shifts the motion's lines (always linewise).
            KeyCode::Char('>') => self.operator(Op::ShiftRight),
            KeyCode::Char('<') => self.operator(Op::ShiftLeft),
            // `=` reindents (bracket-depth): doubled `==` is linewise, `=motion`/`=ap` over a motion.
            KeyCode::Char('=') => self.operator(Op::Reindent),
            // `!{motion}{cmd}<CR>` — the filter operator (linewise, like `=`/`gq`); doubled `!!` (and
            // `{count}!!`) filters the current `count` lines. The frontend numbers the lines and opens the
            // `:{range}!` cmdline.
            KeyCode::Char('!') => self.operator(Op::Filter),
            KeyCode::Char('p') => self.action(Command::Paste {
                after: true,
                count: self.mcount(),
                move_after: false,
            }),
            KeyCode::Char('P') => self.action(Command::Paste {
                after: false,
                count: self.mcount(),
                move_after: false,
            }),
            // Line-operator synonyms: `D`=`d$`, `C`=`c$`, `Y`=`y$` (nvim 0.6+ charwise), `S`=`cc`.
            // Each is the existing operator applied to an implicit motion, routed through the same
            // plan/commit path (so register geometry, cursor clamping, and dot-replayability match).
            KeyCode::Char('D') => self.action(Command::Delete(self.mcount(), Motion::LineEnd)),
            KeyCode::Char('C') => self.action(Command::Change(self.mcount(), Motion::LineEnd)),
            KeyCode::Char('Y') => self.action(Command::Yank(self.mcount(), Motion::LineEnd)),
            KeyCode::Char('S') => self.action(Command::Change(self.mcount(), Motion::Line)),
            // `s` (substitute char) = `cl`: change `count` chars rightward and enter Insert. Same
            // `Change(count, Right)` a `c l` produces, so register geometry and dot-repeat match. Guarded
            // to Normal (no pending operator) so `ds` still aborts — `s` is not a motion.
            KeyCode::Char('s') if self.normal.op.is_none() => {
                self.action(Command::Change(self.mcount(), Motion::Right))
            }
            KeyCode::Char('i') if self.normal.op.is_some() => {
                self.arm_awaiting(Awaiting::TextObjectChar { inner: true })
            }
            KeyCode::Char('a') if self.normal.op.is_some() => {
                self.arm_awaiting(Awaiting::TextObjectChar { inner: false })
            }
            KeyCode::Char('i') => self.insert_entry(Command::EnterInsert),
            KeyCode::Char('a') => self.insert_entry(Command::EnterInsertAfter),
            KeyCode::Char('I') => self.insert_entry(Command::InsertLineStart),
            KeyCode::Char('A') => self.insert_entry(Command::AppendLineEnd),
            KeyCode::Char('o') => self.insert_entry(Command::OpenBelow),
            KeyCode::Char('O') => self.insert_entry(Command::OpenAbove),
            KeyCode::Char('x') => self.action(Command::DeleteUnder(self.mcount())),
            // `X` = `dh`: delete `count` chars before the cursor, never past the line's first char.
            KeyCode::Char('X') if self.normal.op.is_none() => {
                self.action(Command::Delete(self.mcount(), Motion::Left))
            }
            // `<Space>` / `<BS>` move like `l` / `h` but wrap across lines (nvim's default 'whichwrap' is
            // `b,s`); under an operator see [`Motion::WrapRight`] / [`Motion::WrapLeft`].
            KeyCode::Char(' ') if key.modifiers.is_empty() => self.motion(Motion::WrapRight),
            KeyCode::Backspace if key.modifiers.is_empty() => self.motion(Motion::WrapLeft),
            // `{count}<Del>` drops the count's last digit (`:help N<Del>`); a bare `<Del>` is `x`.
            KeyCode::Delete if self.normal.count > 0 => {
                self.normal.count /= 10;
                Feed::Pending
            }
            KeyCode::Delete if self.normal.op.is_none() => self.action(Command::DeleteUnder(1)),
            // `<Insert>` = `i`.
            KeyCode::Insert if self.normal.op.is_none() => self.insert_entry(Command::EnterInsert),
            // `Z` arms `ZZ` / `ZQ`.
            KeyCode::Char('Z') if self.normal.op.is_none() => self.arm_awaiting(Awaiting::ZSecond),
            // Doubled case operators (`guu` / `gUU` / `g~~` / `g??`): the second key repeats the operator to
            // make it linewise. With an operator armed only its doubling key passes the operator-pending
            // guard, so `u`/`U`/`~` here ARE the doubling key; `?` is also the backward search (`d?pat`).
            KeyCode::Char('u' | 'U' | '~') if self.normal.op.is_some() => self.op_linewise(),
            KeyCode::Char('?')
                if matches!(self.normal.op, Some(OpPending { op: Op::Rot13, .. })) =>
            {
                self.op_linewise()
            }
            // `gqq` / `gww` — the doubled reflow operators format the current `count` lines (reachable now
            // that the session's macro layer leaves a `q` to an armed operator, #679). Only the doubling key
            // passes the operator-pending guard, so an armed operator here IS `gq` / `gw`.
            KeyCode::Char('q' | 'w') if self.normal.op.is_some() => self.op_linewise(),
            KeyCode::Char('u') => self.counted(Command::Undo),
            // `U` — undo the latest changes on the last changed line (a count does nothing).
            KeyCode::Char('U') => self.action(Command::LineUndo),
            KeyCode::Tab => self.action(Command::GotoNewerJump),
            KeyCode::Char('r') => self.arm_awaiting(Awaiting::ReplaceChar),
            KeyCode::Char('R') => self.insert_entry(Command::EnterReplace),
            // With 'tildeop' `~` is the `g~` operator (`~w`; `~~` is its line form, above).
            KeyCode::Char('~') if self.tildeop => self.arm_op(Op::CaseToggle),
            KeyCode::Char('~') => self.action(Command::ToggleCase(self.mcount())),
            KeyCode::Char('J') => self.action(Command::JoinLines(self.mcount())),
            // `n` repeats the last search in the SAME direction it was issued; `N` in the OPPOSITE. So
            // after `?foo`, `n` continues BACKWARD and `N` goes forward; after `/foo` they stay
            // forward-relative. Direction comes from the stored last-search flag. Both REAPPLY the stored
            // offset (`:help search-offset`), so they route through [`Command::Search`] (which carries the
            // offset) rather than the plain `SearchNext`/`SearchPrev`; the offset-aware match rule advances
            // past the current match even when the offset landed the cursor before it (`s-1`).
            KeyCode::Char('n') => self.repeat_search(false),
            KeyCode::Char('N') => self.repeat_search(true),
            // `*`/`#` — search the whole keyword under the cursor forward / backward; operator-aware
            // (`d*`). The planner reads the keyword from the buffer (the engine has no buffer).
            KeyCode::Char('*') => self.word_search(true, true),
            KeyCode::Char('#') => self.word_search(false, true),
            // Dot-repeat: replay the last recorded change at the current cursor (D-047). A leading `N`
            // overrides the change's count (Vim `3.`). `.` itself never rewrites the record (beyond the
            // numbered-register step below), so `..` repeats the same change; with no prior change it is a clean no-op (the Normal namespace's
            // `closed/ignore` policy — Vim rings the bell).
            KeyCode::Char('.') => {
                if let Some(intent) = self.last_change.as_mut() {
                    // A change that named `"1`-`"8` repeats with the next numbered register, and the record
                    // keeps it, so `"1p...` walks the delete ring (nvim `do_redo`, #816); `"9` stays.
                    if let Some(c @ '1'..='8') = intent.register {
                        intent.register = char::from_digit(c.to_digit(10).unwrap_or(0) + 1, 10);
                    }
                    let intent = intent.clone();
                    let count = (self.normal.typed() > 0).then_some(self.normal.typed());
                    self.reset();
                    intent.replay(count).into_feed()
                } else {
                    self.unmatched(Ns::Normal, key)
                }
            }
            KeyCode::Char('/') => self.enter_search(false),
            // `?` — open the BACKWARD search line. Reachable here only when no ROT13 operator is pending
            // (the guarded `g??` arm above wins that case); an armed d/c/y operator is captured so `d?pat`
            // works, exactly like `d/pat`.
            KeyCode::Char('?') => self.enter_search(true),
            // `{count}:` opens the line on the count's lines (`Feed::CountCmdline`).
            KeyCode::Char(':') => {
                let count = self.normal.typed();
                self.reset();
                if count > 0 {
                    return Feed::CountCmdline { count };
                }
                self.open_cmdline(':', false);
                Feed::Pending
            }
            // The base namespace's own declared policy — not a shared fallthrough. In Visual this
            // line is unreachable (the Visual arm above returns first), which is why the two are
            // separate calls rather than one `mode`-derived namespace.
            _ => self.unmatched(Ns::Normal, key),
        }
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

    /// The target char after `f`/`F`/`t`/`T`.
    fn await_find_target(&mut self, key: KeyEvent, forward: bool, till: bool) -> Feed {
        match key.code {
            KeyCode::Char(ch) => {
                self.last_find = Some((ch, forward, till));
                self.motion(Motion::FindChar {
                    ch,
                    forward,
                    till,
                    repeat: false,
                })
            }
            // A pending construct is in flight, so this is `closed/abort` — the policy
            // that distinguishes operator-pending from Normal (VS-OBL-3).
            _ => self.unmatched(Ns::OperatorPending, key),
        }
    }

    /// The text-object selector after `i`/`a` (under an operator or in a selection).
    fn await_text_object(&mut self, key: KeyEvent, inner: bool) -> Feed {
        match key.code {
            // Under an operator this composes (`diw`/`da(`/`ci"`); in a selection `self.motion`
            // emits a bare `Move` whose text-object shape the core turns into a selection (`viw`).
            KeyCode::Char(ch) if text_object(ch, inner).is_some() => {
                self.motion(text_object(ch, inner).expect("guarded by is_some"))
            }
            // Not a text-object selector (`t` IS one — `it`/`at` tag objects resolve via
            // `text_object` → `Motion::Tag` above): a pending construct is in flight, so this is
            // `closed/abort` — the operator-pending policy (VS-OBL-3).
            _ => self.unmatched(Ns::OperatorPending, key),
        }
    }

    /// The second key of a `g` command.
    fn await_g_second(&mut self, key: KeyEvent, mode: Mode) -> Feed {
        // `g<Down>`/`g<Up>`/`g<Home>`/`g<End>` are `gj`/`gk`/`g0`/`g$` (`:help g<Down>`).
        let key = match key.code {
            KeyCode::Down if key.modifiers.is_empty() => plain(KeyCode::Char('j')),
            KeyCode::Up if key.modifiers.is_empty() => plain(KeyCode::Char('k')),
            KeyCode::Home if key.modifiers.is_empty() => plain(KeyCode::Char('0')),
            KeyCode::End if key.modifiers.is_empty() => plain(KeyCode::Char('$')),
            _ => key,
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // `g CTRL-G` and Visual `g CTRL-A`/`g CTRL-X` are the only `g` chords; any other one aborts
        // instead of reaching the bare `g{char}` arms (`g<C-e>` is not `ge`).
        let g_chord = match key.code {
            KeyCode::Char('g') => true,
            KeyCode::Char('a' | 'x') => matches!(mode, Mode::Visual { .. }),
            KeyCode::Char('h') => mode == Mode::Normal,
            _ => false,
        };
        if is_chord(key) && matches!(key.code, KeyCode::Char(_)) && !(ctrl && g_chord) {
            return self.unmatched(Ns::OperatorPending, key);
        }
        // Under an operator only the `g` MOTIONS continue it, and the doubled form of the `g`
        // operators (`gugu`, `gUgU`, `g~g~`, `g?g?`, `gqgq`, `gwgw`) is its linewise form; any other
        // `g` command (`dgJ`, `dgp`, `ygu`, ...) aborts instead of running bare (#790).
        if let Some(p) = self.normal.op {
            if let Some(feed) = self.g_under_op(p, key, ctrl) {
                return feed;
            }
        }
        self.g_command(key, mode, ctrl)
    }

    /// The `g` second key under an operator: `None` when it continues the operator as a `g` motion.
    fn g_under_op(&mut self, p: OpPending, key: KeyEvent, ctrl: bool) -> Option<Feed> {
        let KeyCode::Char(c) = key.code else {
            return Some(self.unmatched(Ns::OperatorPending, key));
        };
        let doubled = matches!(
            (p.op, c),
            (Op::CaseLower, 'u')
                | (Op::CaseUpper, 'U')
                | (Op::CaseToggle, '~')
                | (Op::Rot13, '?')
                | (Op::Format, 'q')
                | (Op::FormatKeep, 'w')
        );
        if !ctrl && doubled {
            return Some(self.op_linewise());
        }
        let g_motion = matches!(
            c,
            'g' | 'e'
                | 'E'
                | '_'
                | 'j'
                | 'k'
                | '0'
                | '$'
                | '^'
                | 'm'
                | 'o'
                | 'M'
                | 'n'
                | 'N'
                | '*'
                | '#'
                | '`'
                | '\''
        );
        if ctrl || !g_motion {
            return Some(self.unmatched(Ns::OperatorPending, key));
        }
        None
    }

    /// The char after `r` (Normal) or `v_r` (a selection).
    fn await_replace_char(&mut self, key: KeyEvent, mode: Mode) -> Feed {
        match key.code {
            // In a selection, `r{char}` replaces EVERY selected char with it (Vim `v_r`); in Normal
            // it replaces `count` chars under the cursor. The count accumulated before `r` is still
            // live (the `r` arm did not reset it).
            KeyCode::Char(c) if matches!(mode, Mode::Visual { .. } | Mode::Select { .. }) => {
                self.action(Command::ReplaceSelectionChar(c))
            }
            KeyCode::Char(c) => self.action(Command::ReplaceChar(self.mcount(), c)),
            // `r<CR>` replaces the char(s) with a line break (Vim splits the line). Enter is a
            // distinct KeyCode, so map it to a `\n` replacement here.
            KeyCode::Enter if matches!(mode, Mode::Visual { .. } | Mode::Select { .. }) => {
                self.action(Command::ReplaceSelectionChar('\n'))
            }
            KeyCode::Enter => self.action(Command::ReplaceChar(self.mcount(), '\n')),
            // A pending construct is in flight, so this is `closed/abort` — the policy
            // that distinguishes operator-pending from Normal (VS-OBL-3).
            _ => self.unmatched(Ns::OperatorPending, key),
        }
    }

    /// The char after `gr`.
    fn await_virtual_replace_char(&mut self, key: KeyEvent) -> Feed {
        match key.code {
            // `{count}gr{char}` — one-shot virtual replace (tab-aware) of `count` chars, back to
            // Normal. The count accumulated before `gr` is still live (the `gr` arm did not reset it).
            // No `<CR>` form: classic `gr<CR>` is not the line-splitting `r<CR>` — the char is taken
            // literally, so a bare Enter here simply aborts the pending construct.
            KeyCode::Char(c) => self.action(Command::VirtualReplaceChar(self.mcount(), c)),
            _ => self.unmatched(Ns::OperatorPending, key),
        }
    }

    /// The key after `CTRL-\`.
    fn await_ctrl_backslash(&mut self, key: KeyEvent, mode: Mode) -> Feed {
        let only_ctrl = key.modifiers - KeyModifiers::SHIFT == KeyModifiers::CONTROL;
        match key.code {
            KeyCode::Char('n' | 'g') if only_ctrl => {
                if mode == Mode::Normal {
                    self.reset();
                    Feed::Ignored
                } else {
                    self.action(Command::EnterNormal)
                }
            }
            _ => self.unmatched(Ns::OperatorPending, key),
        }
    }

    /// The key after `Z` (`ZZ` / `ZQ`).
    fn await_z_second(&mut self, key: KeyEvent) -> Feed {
        let line = match key.code {
            KeyCode::Char('Z') if key.modifiers - KeyModifiers::SHIFT == KeyModifiers::NONE => "x",
            KeyCode::Char('Q') if key.modifiers - KeyModifiers::SHIFT == KeyModifiers::NONE => "q!",
            _ => return self.unmatched(Ns::OperatorPending, key),
        };
        self.reset();
        Feed::ExecuteEx(line.to_string())
    }

    /// Whether a mark name after `` ` `` / `'` aborts the armed operator. Only a named mark composes
    /// with an operator here (`` d`a ``); the special marks do not yet, so under an operator they abort
    /// rather than jump with the operator dropped (#790).
    fn mark_aborts_op(&self, key: KeyEvent) -> bool {
        self.normal.op.is_some()
            && !matches!(
                key.code,
                KeyCode::Char('a'..='z' | 'A'..='Z' | '0'..='9' | '^' | '<' | '>')
            )
    }

    /// The mark name after `` ` `` (or `` g` ``): a charwise mark jump.
    fn await_mark_jump(&mut self, key: KeyEvent, keep_jumps: bool) -> Feed {
        let jump = self.normal.op.is_none();
        if self.mark_aborts_op(key) {
            return self.unmatched(Ns::OperatorPending, key);
        }
        let feed = match key.code {
            // `` `^ `` (where Insert stopped), and `` d`< ``/`` d`> `` under an operator (#1088).
            KeyCode::Char('^') => self.mark_op('^', false),
            KeyCode::Char(c @ ('<' | '>')) if self.normal.op.is_some() => self.mark_op(c, false),
            // `` `. `` — jump to the last-change mark.
            KeyCode::Char('.') => self.action(Command::GotoLastChange),
            // `` `` `` / `` `' `` — jump to the context mark (position before the latest jump).
            KeyCode::Char('`') | KeyCode::Char('\'') => self.action(Command::GotoContextMark),
            // `` `[ `` / `` `] `` — jump charwise to the first/last char of the last changed/yanked text.
            KeyCode::Char('[') => self.action(Command::GotoChangeMarkStart),
            KeyCode::Char(']') => self.action(Command::GotoChangeMarkEnd),
            // `` `< `` / `` `> `` — jump charwise to the start/end of the last visual selection.
            KeyCode::Char('<') => self.action(Command::GotoVisualMarkStart),
            KeyCode::Char('>') => self.action(Command::GotoVisualMarkEnd),
            // `` `{a-z} `` — jump to a named mark, or `` d`a ``/`` y`a `` with an operator pending; a
            // numbered one (`` `1 ``, set by `:mark 1`, #1241) the same way.
            KeyCode::Char(c @ ('a'..='z' | 'A'..='Z' | '0'..='9')) => self.mark_op(c, false),
            // Any other mark name is not wired — abort the pending construct.
            _ => self.unmatched(Ns::OperatorPending, key),
        };
        keep_jumps_feed(feed, keep_jumps && jump)
    }

    /// The mark name after `m`.
    fn await_set_mark(&mut self, key: KeyEvent) -> Feed {
        match key.code {
            // `m{a-zA-Z}` — set a named mark at the cursor (an uppercase one stays with this
            // buffer, #1088).
            KeyCode::Char(c @ ('a'..='z' | 'A'..='Z')) => self.action(Command::SetNamedMark(c)),
            // The other marks `m` can set are deferred — abort.
            _ => self.unmatched(Ns::OperatorPending, key),
        }
    }

    /// The mark name after `'` (or `g'`): a linewise mark jump.
    fn await_mark_jump_line(&mut self, key: KeyEvent, keep_jumps: bool) -> Feed {
        let jump = self.normal.op.is_none();
        if self.mark_aborts_op(key) {
            return self.unmatched(Ns::OperatorPending, key);
        }
        let feed = match key.code {
            // `'^`, and `d'<`/`d'>` under an operator (#1088).
            KeyCode::Char('^') => self.mark_op('^', true),
            KeyCode::Char(c @ ('<' | '>')) if self.normal.op.is_some() => self.mark_op(c, true),
            // `'.` — linewise to the last-change line.
            KeyCode::Char('.') => self.action(Command::GotoLastChangeLine),
            // `''` / `` '` `` — linewise to the context mark's line (position before the latest jump).
            KeyCode::Char('\'') | KeyCode::Char('`') => self.action(Command::GotoContextMarkLine),
            // `'[` / `']` — linewise to the first non-blank of the first/last changed/yanked line.
            KeyCode::Char('[') => self.action(Command::GotoChangeMarkStartLine),
            KeyCode::Char(']') => self.action(Command::GotoChangeMarkEndLine),
            // `'<` / `'>` — linewise to the first non-blank of the first/last selected line.
            KeyCode::Char('<') => self.action(Command::GotoVisualMarkStartLine),
            KeyCode::Char('>') => self.action(Command::GotoVisualMarkEndLine),
            // `'{a-z}` — linewise to a named mark's line, or `d'a`/`y'a` with an operator pending; a
            // numbered one (`'1`, #1241) the same way.
            KeyCode::Char(c @ ('a'..='z' | 'A'..='Z' | '0'..='9')) => self.mark_op(c, true),
            // Any other mark name is not wired — abort.
            _ => self.unmatched(Ns::OperatorPending, key),
        };
        keep_jumps_feed(feed, keep_jumps && jump)
    }

    /// The register name after `"`.
    fn await_register(&mut self, key: KeyEvent) -> Feed {
        // `"=` — the expression register (`:help quote=`): open the expression prompt instead of
        // arming a stored slot. On `<CR>` the collected expression is evaluated and the `"=` register
        // armed, so the FOLLOWING `p`/`P` pastes the result. Reset first so no partial Normal state
        // (a stray count/operator) survives into the prompt.
        if key.code == KeyCode::Char('=') {
            self.reset();
            self.open_expr_prompt(ExprTarget::Paste);
            return Feed::Pending;
        }
        match key.code {
            // A register name — a letter (`a`–`z` / `A`–`Z`), the yank register `0`, the numbered
            // delete-ring `1`–`9`, the small-delete `-`, or the blackhole `_`: emit `SetRegister` for the core
            // to hold as the pending register the next yank/delete/change/paste reads. A PREFIX, not
            // a command: the count typed so far survives as its own segment (`3"ayy` yanks 3,
            // `2"a3x` cuts 6) and an `i_CTRL-O` one-shot stays in flight for the command that follows
            // (`i<C-o>"ap` pastes) — #796. `"0p`/`"1p`/`"-p` paste from read-only edit-history slots.
            KeyCode::Char(c)
                if c.is_ascii_alphabetic()
                    || c.is_ascii_digit()
                    || c == '-'
                    || c == '_'
                    // `"+`/`"*` — the system clipboard (`:help quoteplus`).
                    || c == '+'
                    || c == '*'
                    // The read-only special registers `"/ ": ". "%` as a paste SOURCE (`"/p`,
                    // `".p`, `"%p`): arm them like any slot; the following `p`/`P` reads the
                    // frontend-synced value. A yank/delete naming one fails (read-only, #1022).
                    || c == '/'
                    || c == ':'
                    || c == '.'
                    || c == '%'
                    // `"#` — the alternate file name (`:help quote#`).
                    || c == '#' =>
            {
                self.normal.reg_count = mul_raw(self.normal.reg_count, self.normal.count);
                self.normal.count = 0;
                self.normal.register = true;
                Feed::Cmd(Command::SetRegister(Some(c)))
            }
            // Any other name is unusable; a pending construct is in flight, so it is `closed/abort`
            // (operator-pending), leaking no state.
            _ => self.unmatched(Ns::OperatorPending, key),
        }
    }

    /// The key after `[` / `]`.
    fn await_bracket(&mut self, key: KeyEvent, open_bracket: bool) -> Feed {
        // `]p`/`[i`/... are commands, not motions: under an operator they abort (`d]p`, #790).
        if self.normal.op.is_some() && matches!(key.code, KeyCode::Char('p' | 'P' | 'i' | 'I')) {
            return self.unmatched(Ns::OperatorPending, key);
        }
        let count = self.mcount();
        match key.code {
            // Indent-adjusting paste. Only `]p` pastes AFTER (below); `]P`, `[p`, `[P` all paste
            // BEFORE (above), matching Vim. Other bracket commands (`[(`, `]}`, …) are not wired.
            KeyCode::Char('p') => self.action(Command::PasteIndent {
                after: !open_bracket,
                count,
            }),
            KeyCode::Char('P') => self.action(Command::PasteIndent {
                after: false,
                count,
            }),
            // Section motions — count/operator-aware via `self.motion` (which reads the preserved
            // count and composes any armed `d`/`c`/`y`). `]]`/`[[` seek a `{` (or form-feed) in
            // column 0; `][`/`[]` seek a `}` (or form-feed). The starting bracket picks the axis.
            KeyCode::Char(']') if !open_bracket => self.motion(Motion::SectionFwd),
            KeyCode::Char('[') if !open_bracket => self.motion(Motion::SectionEndFwd),
            KeyCode::Char('[') if open_bracket => self.motion(Motion::SectionBack),
            KeyCode::Char(']') if open_bracket => self.motion(Motion::SectionEndBack),
            // Unmatched-paren/brace motions — count/operator-aware via `self.motion`. `[(`/`[{` go to
            // the previous unmatched `(`/`{`; `])`/`]}` go to the next unmatched `)`/`}`. The starting
            // bracket (`[` vs `]`) fixes the direction; the pressed char picks paren vs brace.
            KeyCode::Char('(') if open_bracket => self.motion(Motion::UnmatchedParenBack),
            KeyCode::Char('{') if open_bracket => self.motion(Motion::UnmatchedBraceBack),
            KeyCode::Char(')') if !open_bracket => self.motion(Motion::UnmatchedParenFwd),
            KeyCode::Char('}') if !open_bracket => self.motion(Motion::UnmatchedBraceFwd),
            // Method (brace-block) motions — count/operator-aware via `self.motion`. `]m`/`[m` go to
            // the next/previous method START (a `{`); `]M`/`[M` go to the next/previous method END (a
            // `}`). The starting bracket (`]` vs `[`) fixes the direction; the case picks start vs end.
            KeyCode::Char('m') if !open_bracket => self.motion(Motion::MethodStartFwd),
            KeyCode::Char('m') if open_bracket => self.motion(Motion::MethodStartBack),
            KeyCode::Char('M') if !open_bracket => self.motion(Motion::MethodEndFwd),
            KeyCode::Char('M') if open_bracket => self.motion(Motion::MethodEndBack),
            // Keyword-line lookup (`:help [i`) — DISPLAY the line(s) containing the keyword under
            // the cursor (current buffer). `[i`/`]i` echo the count'th match (`[` from the top of
            // the file, `]` below the cursor); `[I`/`]I` list all matches. The frontend resolves
            // the keyword and scans the lines (the engine has no buffer), like `*`/`gd`.
            KeyCode::Char('i') => self.action(Command::ShowKeywordLines {
                above: open_bracket,
                list: false,
                count,
            }),
            KeyCode::Char('I') => self.action(Command::ShowKeywordLines {
                above: open_bracket,
                list: true,
                count,
            }),
            _ => self.unmatched(Ns::OperatorPending, key),
        }
    }
}
