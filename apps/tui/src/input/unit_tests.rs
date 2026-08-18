#[cfg(test)]
mod tests {
    use crate::input::*;

    pub(super) fn k(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    /// A Meta (Alt) key event — the shared helper for the Emacs-tier tests (`M-f`, `M-d`, …).
    pub(super) fn meta(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
    }
    pub(super) fn feed(seq: &str) -> Feed {
        let mut e = InputEngine::new();
        let mut last = Feed::Ignored;
        for c in seq.chars() {
            last = e.feed(k(c), Mode::Normal);
        }
        last
    }

    #[test]
    fn bare_motions_and_counts() {
        assert_eq!(feed("w"), Feed::Cmd(Command::Move(1, Motion::WordFwd)));
        assert_eq!(feed("3w"), Feed::Cmd(Command::Move(3, Motion::WordFwd)));
        assert_eq!(feed("l"), Feed::Cmd(Command::Move(1, Motion::Right)));
    }

    #[test]
    fn operators_with_counts() {
        assert_eq!(feed("dw"), Feed::Cmd(Command::Delete(1, Motion::WordFwd)));
        assert_eq!(feed("d2w"), Feed::Cmd(Command::Delete(2, Motion::WordFwd)));
        assert_eq!(feed("2dw"), Feed::Cmd(Command::Delete(2, Motion::WordFwd)));
        assert_eq!(feed("2d3w"), Feed::Cmd(Command::Delete(6, Motion::WordFwd)));
    }

    // --- F-027 Lang-Arg translation stage (D-048 / RFC-0013) ---------------------------------------

    fn ctrl_caret() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('^'), KeyModifiers::CONTROL)
    }

    #[test]
    fn lang_stage_translates_in_insert_one_substitution() {
        // Acceptance #1 + INV-FAIL-BOUNDED: a mapped key is rewritten exactly ONCE and dispatched
        // literally. A cyclic map (a<->b) cannot loop — `a` becomes `b`, never re-translated back to `a`.
        let mut e = InputEngine::new();
        e.lang_map.insert('a', 'b');
        e.lang_map.insert('b', 'a');
        e.lang_active = true;
        assert_eq!(
            e.feed(k('a'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('b'))
        );
        assert_eq!(
            e.feed(k('b'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('a'))
        );
    }

    #[test]
    fn lang_stage_inert_outside_the_three_contexts() {
        // Acceptance #2 ("and to nothing else"): Normal/Replace never translate — operators and motions
        // are immune. Map `d`->`w`; in Normal `d` still ARMS the delete operator (Pending), it is NOT
        // rewritten to the `w` motion.
        let mut e = InputEngine::new();
        e.lang_map.insert('d', 'w');
        e.lang_active = true;
        assert_eq!(
            e.translate_lang(k('d'), Mode::Normal).code,
            KeyCode::Char('d')
        );
        assert_eq!(e.feed(k('d'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.translate_lang(k('d'), Mode::Replace).code,
            KeyCode::Char('d')
        );
    }

    #[test]
    fn lang_stage_translates_single_char_argument() {
        // Acceptance #2 (positive half): a command reading a single character (`f`/`r`) has its ARGUMENT
        // translated, regardless of how the command was reached.
        let mut e = InputEngine::new();
        e.lang_map.insert('a', 'x');
        e.lang_active = true;
        // `f a` finds the TRANSLATED char `x`.
        assert_eq!(e.feed(k('f'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.feed(k('a'), Mode::Normal),
            Feed::Cmd(Command::Move(
                1,
                Motion::FindChar {
                    ch: 'x',
                    forward: true,
                    till: false
                }
            ))
        );
        // `r a` replaces with the TRANSLATED char `x`.
        assert_eq!(e.feed(k('r'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.feed(k('a'), Mode::Normal),
            Feed::Cmd(Command::ReplaceChar(1, 'x'))
        );
    }

    #[test]
    fn lang_stage_off_by_default_and_toggled_by_ctrl_caret() {
        // The map is inert until activated (so `:lmap`-defining keystrokes are never rewritten), and
        // `i_CTRL-^` toggles it within Insert (RFC-0013).
        let mut e = InputEngine::new();
        e.lang_map.insert('a', 'б');
        // Off by default: `a` inserts `a`.
        assert_eq!(
            e.feed(k('a'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('a'))
        );
        // CTRL-^ activates (a silent, non-inserting toggle).
        assert_eq!(e.feed(ctrl_caret(), Mode::Insert), Feed::Pending);
        assert_eq!(
            e.feed(k('a'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('б'))
        );
        // CTRL-^ again deactivates.
        assert_eq!(e.feed(ctrl_caret(), Mode::Insert), Feed::Pending);
        assert_eq!(
            e.feed(k('a'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('a'))
        );
    }

    #[test]
    fn lang_stage_translates_on_the_command_line() {
        // The Command-line namespace is Lang-Arg-eligible: a mapped key appends its TRANSLATION to the
        // owned line buffer (F-026 owns the buffer; the stage rewrites the key before it lands there).
        let mut e = InputEngine::new();
        e.lang_map.insert('a', 'б');
        e.lang_active = true;
        assert_eq!(e.feed(k(':'), Mode::Normal), Feed::Pending); // opens the command line
        let _ = e.feed(k('a'), Mode::Normal);
        assert_eq!(e.cmdline.as_ref().map(|c| c.buffer.as_str()), Some("б"));
    }

    #[test]
    fn lang_map_maintenance_methods() {
        // `set_lang_mapping` / `clear_lang_mapping` (the `:lmap` / `:lunmap` back-ends).
        let mut e = InputEngine::new();
        e.set_lang_mapping('a', 'б');
        e.lang_active = true;
        assert_eq!(
            e.feed(k('a'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('б'))
        );
        e.clear_lang_mapping('a');
        assert_eq!(
            e.feed(k('a'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('a'))
        );
    }

    #[test]
    fn parse_lmap_and_lunmap() {
        assert_eq!(
            parse_ex("lmap a б"),
            Ex::Lmap {
                lhs: 'a', rhs: 'б'
            }
        );
        assert_eq!(parse_ex("lunmap a"), Ex::Lunmap { lhs: 'a' });
        // A verb needs whitespace, both sides are single chars, and no trailing tokens — else Unknown.
        assert!(matches!(parse_ex("lmapx"), Ex::Unknown(_)));
        assert!(matches!(parse_ex("lmap a"), Ex::Unknown(_)));
        assert!(matches!(parse_ex("lmap ab cd"), Ex::Unknown(_)));
        assert!(matches!(parse_ex("lmap a b c"), Ex::Unknown(_)));
        assert!(matches!(parse_ex("lunmapx"), Ex::Unknown(_)));
    }

    #[test]
    fn doubled_operator_is_linewise() {
        assert_eq!(feed("dd"), Feed::Cmd(Command::Delete(1, Motion::Line)));
        assert_eq!(feed("2dd"), Feed::Cmd(Command::Delete(2, Motion::Line)));
        assert_eq!(feed("cc"), Feed::Cmd(Command::Change(1, Motion::Line)));
    }

    #[test]
    fn forced_wise_after_operator() {
        // `v`/`V` after an operator FORCE the next motion's wise (Vim o_v/o_V) → OpForced.
        assert_eq!(
            feed("dvj"),
            Feed::Cmd(Command::OpForced {
                op: OpKind::Delete,
                count: 1,
                motion: Motion::Down,
                wise: ForcedWise::Charwise,
            })
        );
        assert_eq!(
            feed("dVe"),
            Feed::Cmd(Command::OpForced {
                op: OpKind::Delete,
                count: 1,
                motion: Motion::WordEnd,
                wise: ForcedWise::Linewise,
            })
        );
        // Count still multiplies through the forced form (`y2Vj`).
        assert_eq!(
            feed("y2Vj"),
            Feed::Cmd(Command::OpForced {
                op: OpKind::Yank,
                count: 2,
                motion: Motion::Down,
                wise: ForcedWise::Linewise,
            })
        );
    }

    #[test]
    fn bare_v_still_enters_visual() {
        // Without an operator armed, `v`/`V` enter Visual as before — the force only applies operator-pending.
        assert_eq!(
            feed("v"),
            Feed::Cmd(Command::EnterVisual {
                kind: SelectKind::Charwise
            })
        );
        assert_eq!(
            feed("V"),
            Feed::Cmd(Command::EnterVisual {
                kind: SelectKind::Linewise
            })
        );
    }

    #[test]
    fn cw_is_ce() {
        assert_eq!(feed("cw"), Feed::Cmd(Command::Change(1, Motion::WordEnd)));
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn emacs_profile_is_non_modal() {
        // F-012 seam: the Emacs profile resolves global-map C- motions to commands and self-inserts a
        // printable key — regardless of the (Vim) mode passed, and without the modal grammar.
        let mut e = InputEngine::emacs();
        assert_eq!(
            e.feed(ctrl('f'), Mode::Insert),
            Feed::Cmd(Command::MoveRight)
        );
        assert_eq!(
            e.feed(ctrl('b'), Mode::Insert),
            Feed::Cmd(Command::MoveLeft)
        );
        assert_eq!(
            e.feed(ctrl('n'), Mode::Insert),
            Feed::Cmd(Command::MoveDown)
        );
        assert_eq!(e.feed(ctrl('p'), Mode::Insert), Feed::Cmd(Command::MoveUp));
        assert_eq!(
            e.feed(ctrl('a'), Mode::Insert),
            Feed::Cmd(Command::MoveLineStart)
        );
        assert_eq!(
            e.feed(ctrl('e'), Mode::Insert),
            Feed::Cmd(Command::MoveLineEnd)
        );
        // A printable key self-inserts (no Insert mode needed) — proof of "move + insert in one state".
        assert_eq!(
            e.feed(k('x'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('x'))
        );
        // `d` is a literal char here, NOT the Vim delete operator (the modal grammar is not consulted).
        assert_eq!(
            e.feed(k('d'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('d'))
        );
        // An unbound control key is inert (not the start of a Vim construct).
        assert_eq!(e.feed(ctrl('z'), Mode::Insert), Feed::Ignored);
    }

    #[test]
    fn emacs_prefix_argument_multiplies_motions_and_repeats_text() {
        // F-012 / D-049: `C-u` seeds a prefix argument (default 4); digits make it explicit; a further
        // `C-u` multiplies by four. The next command consumes it OPAQUELY — a motion multiplies, a
        // self-insert repeats. Each accumulating key is Pending (consumed, not yet a command).
        let mut e = InputEngine::emacs();

        // Bare `C-u` → default 4: `C-u C-f` moves right four graphemes.
        assert_eq!(e.feed(ctrl('u'), Mode::Insert), Feed::Pending);
        assert_eq!(
            e.feed(ctrl('f'), Mode::Insert),
            Feed::Cmd(Command::Move(4, Motion::Right))
        );

        // Explicit decimal: `C-u 3 7 C-b` moves left 37.
        assert_eq!(e.feed(ctrl('u'), Mode::Insert), Feed::Pending);
        assert_eq!(e.feed(k('3'), Mode::Insert), Feed::Pending);
        assert_eq!(e.feed(k('7'), Mode::Insert), Feed::Pending);
        assert_eq!(
            e.feed(ctrl('b'), Mode::Insert),
            Feed::Cmd(Command::Move(37, Motion::Left))
        );

        // `C-u C-u` → 16: a self-insert repeats sixteen times (Replay, not a single Cmd).
        assert_eq!(e.feed(ctrl('u'), Mode::Insert), Feed::Pending);
        assert_eq!(e.feed(ctrl('u'), Mode::Insert), Feed::Pending);
        assert_eq!(
            e.feed(k('a'), Mode::Insert),
            Feed::Replay(vec![Command::InsertChar('a'); 16])
        );

        // count == 1 keeps the grapheme-aware bare motion — the no-argument path is unchanged.
        assert_eq!(
            e.feed(ctrl('f'), Mode::Insert),
            Feed::Cmd(Command::MoveRight)
        );
        // A bare digit with no pending argument is ordinary self-inserting text, not an argument.
        assert_eq!(
            e.feed(k('3'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('3'))
        );
    }

    #[test]
    fn emacs_cx_prefix_map_dispatches_second_key() {
        // F-012: `C-x` opens the extended-command prefix map — the first key is Pending, the second
        // resolves inside the map. `C-x C-s` saves, `C-x C-c` quits, `C-x u` undoes.
        let mut e = InputEngine::emacs();

        assert_eq!(e.feed(ctrl('x'), Mode::Insert), Feed::Pending);
        assert_eq!(e.feed(ctrl('s'), Mode::Insert), Feed::Cmd(Command::Save));

        assert_eq!(e.feed(ctrl('x'), Mode::Insert), Feed::Pending);
        assert_eq!(e.feed(ctrl('c'), Mode::Insert), Feed::Cmd(Command::Quit));

        assert_eq!(e.feed(ctrl('x'), Mode::Insert), Feed::Pending);
        assert_eq!(e.feed(k('u'), Mode::Insert), Feed::Cmd(Command::Undo));

        // An unbound second key (here `C-g`, keyboard-quit) cancels the prefix and is inert — and the engine
        // is not left wedged: the very next key dispatches normally in the global map.
        assert_eq!(e.feed(ctrl('x'), Mode::Insert), Feed::Pending);
        assert_eq!(e.feed(ctrl('g'), Mode::Insert), Feed::Ignored);
        assert_eq!(
            e.feed(ctrl('f'), Mode::Insert),
            Feed::Cmd(Command::MoveRight)
        );
        // A bare `C-s` with no prefix is unbound in the global map (not a save) — prefix scoping holds.
        assert_eq!(e.feed(ctrl('s'), Mode::Insert), Feed::Ignored);
    }

    #[test]
    fn emacs_kill_and_yank_over_the_unnamed_register() {
        // F-012 / D-026: `C-k` kills into the unnamed register (the depth-1 kill ring) via `EmacsKillLine`
        // (D-051) — its own command, not Vim's `Delete(1, LineEnd)`, because at EOL it kills the newline.
        // `C-y` is Emacs yank (D-051, `EmacsYank`) — paste + set the mark, distinct from Vim `P`/`Paste`;
        // it honours a prefix count.
        let mut e = InputEngine::emacs();

        assert_eq!(
            e.feed(ctrl('k'), Mode::Insert),
            Feed::Cmd(Command::EmacsKillLine)
        );
        assert_eq!(
            e.feed(ctrl('y'), Mode::Insert),
            Feed::Cmd(Command::EmacsYank { count: 1 })
        );
        // `C-u 3 C-y` yanks three copies — the prefix argument is the yank's repeat count.
        assert_eq!(e.feed(ctrl('u'), Mode::Insert), Feed::Pending);
        assert_eq!(e.feed(k('3'), Mode::Insert), Feed::Pending);
        assert_eq!(
            e.feed(ctrl('y'), Mode::Insert),
            Feed::Cmd(Command::EmacsYank { count: 3 })
        );
    }

    #[test]
    fn emacs_shift_tracked_only_for_non_char_keys() {
        let mut e = InputEngine::emacs();
        // Shift IS meaningful on a non-char key: C-S-<backspace> is kill-whole-line.
        let cs_bs = KeyEvent::new(
            KeyCode::Backspace,
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert_eq!(
            e.feed(cs_bs, Mode::Insert),
            Feed::Cmd(Command::EmacsKillWholeLine)
        );
        // A printable key that arrives WITH a Shift modifier still matches its unshifted binding — Shift is
        // folded into the char, so M-@ (incidental Shift) is still mark-word, not a lookup miss.
        let m_at_shift = KeyEvent::new(KeyCode::Char('@'), KeyModifiers::ALT | KeyModifiers::SHIFT);
        assert_eq!(
            e.feed(m_at_shift, Mode::Insert),
            Feed::Cmd(Command::EmacsMarkWord)
        );
        // Plain M-DEL (no shift) stays backward-kill-word — the new C-S-<backspace> does not shadow it.
        let m_del = KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT);
        assert_eq!(
            e.feed(m_del, Mode::Insert),
            Feed::Cmd(Command::EmacsBackwardKillWord { count: 1 })
        );
    }

    #[test]
    fn emacs_meta_tier_word_motions_and_buffer_ends() {
        // F-012: the Meta (`M-`, Alt) tier — word motions and buffer ends. Word motions honour the prefix
        // count; `M-<`/`M->` (buffer ends) ignore it. A plain `f` still self-inserts (no Alt, no C-).
        let mut e = InputEngine::emacs();

        // `M-f` (Emacs forward-word) rests point AFTER the word (between-char), so it uses EmacsWordFwd,
        // not Vim `w`/WordFwd (which jumps to the next word start).
        assert_eq!(
            e.feed(meta('f'), Mode::Insert),
            Feed::Cmd(Command::Move(1, Motion::EmacsWordFwd))
        );
        assert_eq!(
            e.feed(meta('b'), Mode::Insert),
            Feed::Cmd(Command::Move(1, Motion::WordBack))
        );
        // `M-d` kills a word forward into the register (kill ring) — its own `EmacsKillWord` command (D-051),
        // distinct from Vim `Delete` so consecutive Emacs kills accumulate and Vim deletes never do.
        assert_eq!(
            e.feed(meta('d'), Mode::Insert),
            Feed::Cmd(Command::EmacsKillWord { count: 1 })
        );
        // Buffer ends: `M-<` to the absolute buffer start, `M->` to the absolute end — count-agnostic, and
        // each pushes the mark (D-051, `EmacsBufferEdge`), distinct from Vim `gg`/`G`.
        assert_eq!(
            e.feed(meta('<'), Mode::Insert),
            Feed::Cmd(Command::EmacsBufferEdge { start: true })
        );
        assert_eq!(
            e.feed(meta('>'), Mode::Insert),
            Feed::Cmd(Command::EmacsBufferEdge { start: false })
        );

        // The prefix argument multiplies a word motion: `C-u M-f` = forward four words.
        assert_eq!(e.feed(ctrl('u'), Mode::Insert), Feed::Pending);
        assert_eq!(
            e.feed(meta('f'), Mode::Insert),
            Feed::Cmd(Command::Move(4, Motion::EmacsWordFwd))
        );

        // A plain printable key (no Alt) still self-inserts — the Meta tier does not shadow text entry.
        assert_eq!(
            e.feed(k('f'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('f'))
        );
        // An unbound Meta key is inert (not self-inserted).
        assert_eq!(e.feed(meta('z'), Mode::Insert), Feed::Ignored);
    }

    #[test]
    fn emacs_essential_editing_keys() {
        // F-012: the editing keys that make the profile usable — RET / C-j newline, DEL / C-d delete,
        // C-/ and C-_ undo. Repeatable keys honour the prefix count via Replay.
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let backspace = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        let mut e = InputEngine::emacs();

        // RET and C-j both insert a newline.
        assert_eq!(
            e.feed(enter, Mode::Insert),
            Feed::Cmd(Command::InsertNewline)
        );
        assert_eq!(
            e.feed(ctrl('j'), Mode::Insert),
            Feed::Cmd(Command::InsertNewline)
        );
        // DEL deletes the char before point; C-d is Emacs `delete-char` — deletes forward without filling
        // the kill ring (D-026: DeleteForward, not the yanking Vim `x`/DeleteUnder), with a count.
        assert_eq!(
            e.feed(backspace, Mode::Insert),
            Feed::Cmd(Command::DeleteBack)
        );
        assert_eq!(
            e.feed(ctrl('d'), Mode::Insert),
            Feed::Cmd(Command::DeleteForward(1))
        );
        // C-/ and C-_ are undo.
        assert_eq!(e.feed(ctrl('/'), Mode::Insert), Feed::Cmd(Command::Undo));
        assert_eq!(e.feed(ctrl('_'), Mode::Insert), Feed::Cmd(Command::Undo));

        // Counts: `C-u 2 RET` inserts two newlines (Replay); `C-u 3 C-d` deletes three chars (native count).
        assert_eq!(e.feed(ctrl('u'), Mode::Insert), Feed::Pending);
        assert_eq!(e.feed(k('2'), Mode::Insert), Feed::Pending);
        assert_eq!(
            e.feed(enter, Mode::Insert),
            Feed::Replay(vec![Command::InsertNewline; 2])
        );
        assert_eq!(e.feed(ctrl('u'), Mode::Insert), Feed::Pending);
        assert_eq!(e.feed(k('3'), Mode::Insert), Feed::Pending);
        assert_eq!(
            e.feed(ctrl('d'), Mode::Insert),
            Feed::Cmd(Command::DeleteForward(3))
        );
    }

    #[test]
    fn emacs_mark_and_region_bindings() {
        // F-012 / D-027: C-SPC set-mark, C-w kill-region, M-w kill-ring-save, C-x C-x exchange-point-mark.
        let ctrl_space = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL);
        let mut e = InputEngine::emacs();

        assert_eq!(
            e.feed(ctrl_space, Mode::Insert),
            Feed::Cmd(Command::SetMark)
        );
        assert_eq!(
            e.feed(ctrl('w'), Mode::Insert),
            Feed::Cmd(Command::KillRegion)
        );
        assert_eq!(
            e.feed(meta('w'), Mode::Insert),
            Feed::Cmd(Command::CopyRegion)
        );
        // C-x C-x resolves inside the C-x prefix map (first key Pending).
        assert_eq!(e.feed(ctrl('x'), Mode::Insert), Feed::Pending);
        assert_eq!(
            e.feed(ctrl('x'), Mode::Insert),
            Feed::Cmd(Command::ExchangePointMark)
        );
    }

    #[test]
    fn emacs_mx_reads_a_command_name_and_runs_it() {
        // F-012: M-x opens the minibuffer; typing a command name is Pending; <CR> resolves it via the
        // registry into a Command. An unknown name is inert.
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);

        let mut e = InputEngine::emacs();
        assert_eq!(e.feed(meta('x'), Mode::Insert), Feed::Pending);
        for c in "save-buffer".chars() {
            assert_eq!(e.feed(k(c), Mode::Insert), Feed::Pending);
        }
        assert_eq!(e.feed(enter, Mode::Insert), Feed::Cmd(Command::Save));

        // A named region command routes through M-x too.
        assert_eq!(e.feed(meta('x'), Mode::Insert), Feed::Pending);
        for c in "kill-region".chars() {
            assert_eq!(e.feed(k(c), Mode::Insert), Feed::Pending);
        }
        assert_eq!(e.feed(enter, Mode::Insert), Feed::Cmd(Command::KillRegion));

        // An unknown command name is a no-op (Emacs "[No match]").
        assert_eq!(e.feed(meta('x'), Mode::Insert), Feed::Pending);
        for c in "no-such-command".chars() {
            assert_eq!(e.feed(k(c), Mode::Insert), Feed::Pending);
        }
        assert_eq!(e.feed(enter, Mode::Insert), Feed::Ignored);
        // After the minibuffer closes, normal dispatch resumes.
        assert_eq!(
            e.feed(ctrl('f'), Mode::Insert),
            Feed::Cmd(Command::MoveRight)
        );
    }

    #[test]
    fn emacs_profile_is_a_nine_tier_stack() {
        // F-012 / D-045: the Emacs profile is ONE nine-tier LayerStack (not Vim's separate sealed
        // namespaces), walked highest-priority first down to global-map. This locks the promotion so a
        // later edit cannot silently drop a tier or reorder the walk.
        let e = InputEngine::emacs();
        assert_eq!(e.emacs.map.depth(), 9);
        let order: Vec<&str> = e.emacs.map.order().collect();
        assert_eq!(
            order.first().copied(),
            Some("emacs.keymaptier.01.overriding-terminal-local-map")
        );
        assert_eq!(
            order.last().copied(),
            Some("emacs.keymaptier.09.global-map")
        );
        // global-map carries every binding; the eight upper tiers are transparent (no minor/major modes).
        assert_eq!(
            e.emacs
                .map
                .layer("emacs.keymaptier.09.global-map")
                .map(|l| l.is_empty()),
            Some(false)
        );
        assert_eq!(
            e.emacs
                .map
                .layer("emacs.keymaptier.01.overriding-terminal-local-map")
                .map(|l| l.is_empty()),
            Some(true)
        );
    }

    #[test]
    fn ctrl_v_enters_blockwise_visual() {
        let mut e = InputEngine::new();
        assert_eq!(
            e.feed(ctrl('v'), Mode::Normal),
            Feed::Cmd(Command::EnterVisual {
                kind: SelectKind::Blockwise
            })
        );
    }

    #[test]
    fn v_slash_capital_v_slash_ctrl_v_switch_shape_or_leave() {
        let mut e = InputEngine::new();
        // From charwise: CTRL-V → blockwise, V → linewise, v → leave (Normal).
        let vis = Mode::Visual {
            kind: SelectKind::Charwise,
        };
        assert_eq!(
            e.feed(ctrl('v'), vis),
            Feed::Cmd(Command::EnterVisual {
                kind: SelectKind::Blockwise
            })
        );
        assert_eq!(e.feed(k('v'), vis), Feed::Cmd(Command::EnterNormal));
        // From blockwise: CTRL-V leaves, v → charwise, V → linewise.
        let blk = Mode::Visual {
            kind: SelectKind::Blockwise,
        };
        assert_eq!(e.feed(ctrl('v'), blk), Feed::Cmd(Command::EnterNormal));
        assert_eq!(
            e.feed(k('v'), blk),
            Feed::Cmd(Command::EnterVisual {
                kind: SelectKind::Charwise
            })
        );
        assert_eq!(
            e.feed(k('V'), blk),
            Feed::Cmd(Command::EnterVisual {
                kind: SelectKind::Linewise
            })
        );
    }

    #[test]
    fn block_selection_operators_route_like_any_selection() {
        let mut e = InputEngine::new();
        let blk = Mode::Visual {
            kind: SelectKind::Blockwise,
        };
        assert_eq!(e.feed(k('d'), blk), Feed::Cmd(Command::DeleteSelection));
        assert_eq!(e.feed(k('y'), blk), Feed::Cmd(Command::YankSelection));
    }

    #[test]
    fn block_mode_i_a_c_arm_insert_replicate() {
        let mut e = InputEngine::new();
        let blk = Mode::Visual {
            kind: SelectKind::Blockwise,
        };
        assert_eq!(
            e.feed(KeyEvent::new(KeyCode::Char('I'), KeyModifiers::NONE), blk),
            Feed::Cmd(Command::BlockInsert(BlockInsertKind::Insert))
        );
        assert_eq!(
            e.feed(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE), blk),
            Feed::Cmd(Command::BlockInsert(BlockInsertKind::Append))
        );
        assert_eq!(
            e.feed(k('c'), blk),
            Feed::Cmd(Command::BlockInsert(BlockInsertKind::Change))
        );
        assert_eq!(
            e.feed(k('s'), blk),
            Feed::Cmd(Command::BlockInsert(BlockInsertKind::Change))
        );
    }

    #[test]
    fn charwise_c_is_still_a_plain_change_and_lowercase_i_is_a_text_object() {
        let mut e = InputEngine::new();
        let vis = Mode::Visual {
            kind: SelectKind::Charwise,
        };
        // In charwise/linewise, `c` is the ordinary selection change (not a block-insert).
        assert_eq!(e.feed(k('c'), vis), Feed::Cmd(Command::ChangeSelection));
        // Lowercase `i` begins a text object in every shape (awaits the object key).
        let blk = Mode::Visual {
            kind: SelectKind::Blockwise,
        };
        assert_eq!(e.feed(k('i'), blk), Feed::Pending);
    }

    #[test]
    fn ctrl_v_after_operator_forces_blockwise() {
        // `d<C-v>j`: CTRL-V operator-pending forces the next motion blockwise (Vim o_CTRL-V).
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('d'), Mode::Normal), Feed::Pending);
        assert_eq!(e.feed(ctrl('v'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.feed(k('j'), Mode::Normal),
            Feed::Cmd(Command::OpForced {
                op: OpKind::Delete,
                count: 1,
                motion: Motion::Down,
                wise: ForcedWise::Blockwise,
            })
        );
    }

    #[test]
    fn ctrl_o_runs_one_normal_command_then_returns_to_insert() {
        // In Insert, CTRL-O arms a one-shot (Pending); the next key resolves through the NORMAL grammar
        // (here `x` → DeleteUnder), then the engine returns to plain Insert routing.
        let mut e = InputEngine::new();
        assert_eq!(e.feed(ctrl('o'), Mode::Insert), Feed::Pending);
        assert_eq!(
            e.feed(k('x'), Mode::Insert),
            Feed::Cmd(Command::DeleteUnder(1))
        );
        // Disarmed: the next key is an inserted char again, not a Normal command.
        assert_eq!(
            e.feed(k('x'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('x'))
        );
    }

    #[test]
    fn ctrl_o_spans_a_multi_key_normal_command() {
        // A one-shot survives the intermediate Pending keys of a multi-key command (`dw`), disarming only
        // when the command completes.
        let mut e = InputEngine::new();
        assert_eq!(e.feed(ctrl('o'), Mode::Insert), Feed::Pending);
        assert_eq!(e.feed(k('d'), Mode::Insert), Feed::Pending); // operator armed
        assert_eq!(
            e.feed(k('w'), Mode::Insert),
            Feed::Cmd(Command::Delete(1, Motion::WordFwd))
        );
        assert_eq!(
            e.feed(k('z'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('z')),
            "back to Insert after the one-shot command completes"
        );
    }

    #[test]
    fn ctrl_g_u_breaks_undo_and_other_second_keys_abort() {
        // CTRL-G is a one-key prefix in Insert: `u` (or `U`) emits BreakUndo.
        let mut e = InputEngine::new();
        assert_eq!(e.feed(ctrl('g'), Mode::Insert), Feed::Pending);
        assert_eq!(e.feed(k('u'), Mode::Insert), Feed::Cmd(Command::BreakUndo));
        // A non-`u` second key aborts the prefix without inserting; Insert then resumes normally.
        assert_eq!(e.feed(ctrl('g'), Mode::Insert), Feed::Pending);
        assert_eq!(e.feed(k('x'), Mode::Insert), Feed::Ignored);
        assert_eq!(
            e.feed(k('y'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('y'))
        );
    }

    pub(super) fn esc() -> KeyEvent {
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
    }

    fn fc(ch: char, forward: bool, till: bool) -> Motion {
        Motion::FindChar { ch, forward, till }
    }

    #[test]
    fn char_search_bare_and_operator() {
        assert_eq!(
            feed("fx"),
            Feed::Cmd(Command::Move(1, fc('x', true, false)))
        );
        assert_eq!(feed("tx"), Feed::Cmd(Command::Move(1, fc('x', true, true))));
        assert_eq!(
            feed("Fx"),
            Feed::Cmd(Command::Move(1, fc('x', false, false)))
        );
        assert_eq!(
            feed("Tx"),
            Feed::Cmd(Command::Move(1, fc('x', false, true)))
        );
        assert_eq!(
            feed("2fx"),
            Feed::Cmd(Command::Move(2, fc('x', true, false)))
        );
        // operator targets
        assert_eq!(
            feed("dtx"),
            Feed::Cmd(Command::Delete(1, fc('x', true, true)))
        );
        assert_eq!(
            feed("d2fx"),
            Feed::Cmd(Command::Delete(2, fc('x', true, false)))
        );
    }

    #[test]
    fn char_search_is_pending_until_the_target() {
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('f'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.feed(k('q'), Mode::Normal),
            Feed::Cmd(Command::Move(1, fc('q', true, false)))
        );
    }

    #[test]
    fn semicolon_repeats_comma_reverses() {
        let mut e = InputEngine::new();
        e.feed(k('f'), Mode::Normal);
        e.feed(k('x'), Mode::Normal); // last_find = (x, forward, not-till)
        assert_eq!(
            e.feed(k(';'), Mode::Normal),
            Feed::Cmd(Command::Move(1, fc('x', true, false))),
            "; repeats the last find"
        );
        assert_eq!(
            e.feed(k(','), Mode::Normal),
            Feed::Cmd(Command::Move(1, fc('x', false, false))),
            ", repeats reversed"
        );
    }

    #[test]
    fn line_jumps() {
        assert_eq!(feed("gg"), Feed::Cmd(Command::Move(1, Motion::GotoLine)));
        assert_eq!(feed("5gg"), Feed::Cmd(Command::Move(5, Motion::GotoLine)));
        assert_eq!(feed("G"), Feed::Cmd(Command::Move(1, Motion::LastLine)));
        assert_eq!(feed("5G"), Feed::Cmd(Command::Move(5, Motion::GotoLine)));
        // operator + line jump is linewise
        assert_eq!(feed("dG"), Feed::Cmd(Command::Delete(1, Motion::LastLine)));
        assert_eq!(feed("dgg"), Feed::Cmd(Command::Delete(1, Motion::GotoLine)));
    }

    #[test]
    fn single_key_edits() {
        // `r` is pending until the replacement char; ctrl-r is still redo.
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('r'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.feed(k('z'), Mode::Normal),
            Feed::Cmd(Command::ReplaceChar(1, 'z'))
        );
        assert_eq!(feed("x"), Feed::Cmd(Command::DeleteUnder(1)));
        assert_eq!(feed("~"), Feed::Cmd(Command::ToggleCase(1)));
        assert_eq!(feed("J"), Feed::Cmd(Command::JoinLines));
        // Counts multiply the single-key actions (Vim `3x` / `3~` / `3rz`).
        assert_eq!(feed("3x"), Feed::Cmd(Command::DeleteUnder(3)));
        assert_eq!(feed("3~"), Feed::Cmd(Command::ToggleCase(3)));
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('3'), Mode::Normal), Feed::Pending);
        assert_eq!(e.feed(k('r'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.feed(k('z'), Mode::Normal),
            Feed::Cmd(Command::ReplaceChar(3, 'z'))
        );
        let mut e = InputEngine::new();
        assert_eq!(
            e.feed(
                KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
                Mode::Normal
            ),
            Feed::Cmd(Command::Redo)
        );
    }

    #[test]
    fn shift_operators_doubled_and_counted() {
        // `>>` / `<<` are the doubled linewise forms; the count before them is the line count.
        assert_eq!(feed(">>"), Feed::Cmd(Command::ShiftRight(1)));
        assert_eq!(feed("<<"), Feed::Cmd(Command::ShiftLeft(1)));
        assert_eq!(feed("3>>"), Feed::Cmd(Command::ShiftRight(3)));
        assert_eq!(feed("2<<"), Feed::Cmd(Command::ShiftLeft(2)));
    }

    #[test]
    fn lone_shift_is_pending_then_aborts_on_mismatch() {
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('>'), Mode::Normal), Feed::Pending);
        // A mismatched second bracket aborts cleanly (operator-pending), leaking no state.
        assert_eq!(e.feed(k('<'), Mode::Normal), Feed::Ignored);
        assert!(
            e.normal.op.is_none() && e.normal.awaiting == Awaiting::Nothing && e.normal.count == 0
        );
    }

    #[test]
    fn insert_entry_keys() {
        assert_eq!(feed("o"), Feed::Cmd(Command::OpenBelow));
        assert_eq!(feed("O"), Feed::Cmd(Command::OpenAbove));
        assert_eq!(feed("A"), Feed::Cmd(Command::AppendLineEnd));
        assert_eq!(feed("I"), Feed::Cmd(Command::InsertLineStart));
    }

    #[test]
    fn big_word_motions_and_cw_is_ce() {
        assert_eq!(feed("W"), Feed::Cmd(Command::Move(1, Motion::BigWordFwd)));
        assert_eq!(feed("B"), Feed::Cmd(Command::Move(1, Motion::BigWordBack)));
        assert_eq!(
            feed("dE"),
            Feed::Cmd(Command::Delete(1, Motion::BigWordEnd))
        );
        // `cw`/`cW` behave like `ce`/`cE`.
        assert_eq!(feed("cw"), Feed::Cmd(Command::Change(1, Motion::WordEnd)));
        assert_eq!(
            feed("cW"),
            Feed::Cmd(Command::Change(1, Motion::BigWordEnd))
        );
    }

    #[test]
    fn bracket_match() {
        assert_eq!(feed("%"), Feed::Cmd(Command::Move(1, Motion::MatchBracket)));
        assert_eq!(
            feed("d%"),
            Feed::Cmd(Command::Delete(1, Motion::MatchBracket))
        );
    }

    #[test]
    fn lone_g_is_pending_then_cancels_on_non_g() {
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('g'), Mode::Normal), Feed::Pending);
        assert_eq!(e.feed(k('z'), Mode::Normal), Feed::Ignored);
    }

    #[test]
    fn char_search_extends_visual() {
        let mut e = InputEngine::new();
        let vis = Mode::Visual {
            kind: SelectKind::Charwise,
        };
        assert_eq!(e.feed(k('f'), vis), Feed::Pending);
        assert_eq!(
            e.feed(k(')'), vis),
            Feed::Cmd(Command::Move(1, fc(')', true, false))),
            "f in Visual is a bare move that extends the selection"
        );
    }

    #[test]
    fn enters_visual_from_normal() {
        assert_eq!(
            feed("v"),
            Feed::Cmd(Command::EnterVisual {
                kind: SelectKind::Charwise
            })
        );
        assert_eq!(
            feed("V"),
            Feed::Cmd(Command::EnterVisual {
                kind: SelectKind::Linewise
            })
        );
    }

    #[test]
    fn visual_operators_act_on_the_selection() {
        let mut e = InputEngine::new();
        let vis = Mode::Visual {
            kind: SelectKind::Charwise,
        };
        assert_eq!(e.feed(k('d'), vis), Feed::Cmd(Command::DeleteSelection));
        assert_eq!(e.feed(k('y'), vis), Feed::Cmd(Command::YankSelection));
        assert_eq!(e.feed(k('c'), vis), Feed::Cmd(Command::ChangeSelection));
        assert_eq!(e.feed(k('x'), vis), Feed::Cmd(Command::DeleteSelection));
        assert_eq!(e.feed(esc(), vis), Feed::Cmd(Command::EnterNormal));
    }

    #[test]
    fn visual_o_swaps_selection_ends() {
        // In Visual/Select, `o` emits SwapSelectionEnds (in Normal it is OpenBelow).
        let mut e = InputEngine::new();
        assert_eq!(
            e.feed(
                k('o'),
                Mode::Visual {
                    kind: SelectKind::Charwise
                }
            ),
            Feed::Cmd(Command::SwapSelectionEnds)
        );
        assert_eq!(
            e.feed(
                k('o'),
                Mode::Select {
                    kind: SelectKind::Linewise
                }
            ),
            Feed::Cmd(Command::SwapSelectionEnds)
        );
        // Sanity: `o` in Normal is still OpenBelow.
        assert_eq!(feed("o"), Feed::Cmd(Command::OpenBelow));
    }

    #[test]
    fn visual_motion_extends_selection() {
        let mut e = InputEngine::new();
        let vis = Mode::Visual {
            kind: SelectKind::Charwise,
        };
        // A bare motion in Visual is a Move (no operator) — the frontend re-plans it against the anchor.
        assert_eq!(
            e.feed(k('l'), vis),
            Feed::Cmd(Command::Move(1, Motion::Right))
        );
        assert_eq!(
            e.feed(k('w'), vis),
            Feed::Cmd(Command::Move(1, Motion::WordFwd))
        );
    }

    #[test]
    fn visual_toggle_and_switch() {
        let mut e = InputEngine::new();
        // `v` in charwise Visual exits; `V` switches it to linewise.
        assert_eq!(
            e.feed(
                k('v'),
                Mode::Visual {
                    kind: SelectKind::Charwise
                }
            ),
            Feed::Cmd(Command::EnterNormal)
        );
        assert_eq!(
            e.feed(
                k('V'),
                Mode::Visual {
                    kind: SelectKind::Charwise
                }
            ),
            Feed::Cmd(Command::EnterVisual {
                kind: SelectKind::Linewise
            })
        );
        assert_eq!(
            e.feed(
                k('v'),
                Mode::Visual {
                    kind: SelectKind::Linewise
                }
            ),
            Feed::Cmd(Command::EnterVisual {
                kind: SelectKind::Charwise
            })
        );
    }

    fn ctrl_g() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL)
    }

    #[test]
    fn ctrl_g_toggles_visual_and_select_both_ways() {
        let mut e = InputEngine::new();
        // Visual -> Select, carrying the charwise/linewise shape.
        assert_eq!(
            e.feed(
                ctrl_g(),
                Mode::Visual {
                    kind: SelectKind::Charwise
                }
            ),
            Feed::Cmd(Command::EnterSelect {
                kind: SelectKind::Charwise
            })
        );
        assert_eq!(
            e.feed(
                ctrl_g(),
                Mode::Visual {
                    kind: SelectKind::Linewise
                }
            ),
            Feed::Cmd(Command::EnterSelect {
                kind: SelectKind::Linewise
            })
        );
        // Select -> Visual, back again.
        assert_eq!(
            e.feed(
                ctrl_g(),
                Mode::Select {
                    kind: SelectKind::Charwise
                }
            ),
            Feed::Cmd(Command::EnterVisual {
                kind: SelectKind::Charwise
            })
        );
        assert_eq!(
            e.feed(
                ctrl_g(),
                Mode::Select {
                    kind: SelectKind::Linewise
                }
            ),
            Feed::Cmd(Command::EnterVisual {
                kind: SelectKind::Linewise
            })
        );
        // CTRL-G is inert in Normal (no selection to toggle); it is NOT the start of `gg`.
        assert_eq!(e.feed(ctrl_g(), Mode::Normal), Feed::Ignored);
    }

    #[test]
    fn printable_key_in_select_replaces_the_selection() {
        // A key that matches no motion/operator hits Select's `open/replace-selection` policy.
        let sel = Mode::Select {
            kind: SelectKind::Charwise,
        };
        let mut e = InputEngine::new();
        assert_eq!(
            e.feed(k('z'), sel),
            Feed::Cmd(Command::ReplaceSelection('z'))
        );
        // A non-printable unmatched key does nothing.
        assert_eq!(e.feed(esc(), sel), Feed::Cmd(Command::EnterNormal));
        let mut e = InputEngine::new();
        assert_eq!(
            e.feed(k('A'), sel),
            Feed::Cmd(Command::ReplaceSelection('A'))
        );
    }

    #[test]
    fn select_operators_and_motions_match_visual() {
        let sel = Mode::Select {
            kind: SelectKind::Charwise,
        };
        let mut e = InputEngine::new();
        // d/y/c act on the selection, exactly as in Visual.
        assert_eq!(e.feed(k('d'), sel), Feed::Cmd(Command::DeleteSelection));
        assert_eq!(e.feed(k('y'), sel), Feed::Cmd(Command::YankSelection));
        assert_eq!(e.feed(k('c'), sel), Feed::Cmd(Command::ChangeSelection));
        // A motion extends the selection (a bare Move; the frontend re-plans it against the anchor).
        assert_eq!(
            e.feed(k('l'), sel),
            Feed::Cmd(Command::Move(1, Motion::Right))
        );
        assert_eq!(
            e.feed(k('w'), sel),
            Feed::Cmd(Command::Move(1, Motion::WordFwd))
        );
        // Esc leaves the selection.
        assert_eq!(e.feed(esc(), sel), Feed::Cmd(Command::EnterNormal));
    }

    #[test]
    fn named_register_prefix_parses() {
        // `"a` is pending until the name, then emits SetRegister; the following op is unaffected.
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('"'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.feed(k('a'), Mode::Normal),
            Feed::Cmd(Command::SetRegister(Some('a')))
        );
        // `"a3yy` → the count typed after the register prefix still lands.
        let mut e = InputEngine::new();
        e.feed(k('"'), Mode::Normal);
        e.feed(k('a'), Mode::Normal);
        e.feed(k('3'), Mode::Normal);
        e.feed(k('y'), Mode::Normal);
        assert_eq!(
            e.feed(k('y'), Mode::Normal),
            Feed::Cmd(Command::Yank(3, Motion::Line)),
            "count after the register prefix still applies"
        );
        // Uppercase names (append) parse too.
        let mut e = InputEngine::new();
        e.feed(k('"'), Mode::Normal);
        assert_eq!(
            e.feed(k('A'), Mode::Normal),
            Feed::Cmd(Command::SetRegister(Some('A')))
        );
        // `"` works in Visual as well (Vim `"xy`).
        let mut e = InputEngine::new();
        let vis = Mode::Visual {
            kind: SelectKind::Charwise,
        };
        assert_eq!(e.feed(k('"'), vis), Feed::Pending);
        assert_eq!(
            e.feed(k('a'), vis),
            Feed::Cmd(Command::SetRegister(Some('a')))
        );
    }

    #[test]
    fn yank_operator_and_paste() {
        assert_eq!(feed("yw"), Feed::Cmd(Command::Yank(1, Motion::WordFwd)));
        assert_eq!(feed("y2w"), Feed::Cmd(Command::Yank(2, Motion::WordFwd)));
        assert_eq!(feed("yy"), Feed::Cmd(Command::Yank(1, Motion::Line)));
        assert_eq!(feed("2yy"), Feed::Cmd(Command::Yank(2, Motion::Line)));
        assert_eq!(feed("yiw"), Feed::Cmd(Command::Yank(1, Motion::InnerWord)));
        assert_eq!(
            feed("p"),
            Feed::Cmd(Command::Paste {
                after: true,
                count: 1
            })
        );
        assert_eq!(
            feed("P"),
            Feed::Cmd(Command::Paste {
                after: false,
                count: 1
            })
        );
    }

    #[test]
    fn pending_states() {
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('2'), Mode::Normal), Feed::Pending);
        assert_eq!(e.feed(k('d'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.feed(k('w'), Mode::Normal),
            Feed::Cmd(Command::Delete(2, Motion::WordFwd))
        );
    }

    #[test]
    fn insert_mode_and_ex() {
        let mut e = InputEngine::new();
        assert_eq!(
            e.feed(k('z'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('z'))
        );
        assert_eq!(e.feed(k(':'), Mode::Normal), Feed::Pending);
        assert_eq!(parse_ex("wq"), Ex::SaveQuit);
        assert_eq!(
            parse_ex("trace save t.trace"),
            Ex::SaveTrace("t.trace".into())
        );
    }
}

/// Dot-repeat (`.`): the engine records the last change as a re-parameterizable [`ChangeIntent`] and
/// replays it — the operator/edit at the CURRENT cursor, plus any captured insert text (F-023).
#[cfg(test)]
mod dot_repeat_tests {
    use super::tests::{esc, k};
    use crate::input::*;

    /// Feed a whole sequence, tracking the mode the way the frontend does (a completed command may change
    /// the mode, which the next key must see). Only the outcome matters here; we assert on the LAST feed.
    fn feed_modes(seq: &[KeyEvent]) -> (InputEngine, Feed) {
        let mut e = InputEngine::new();
        let mut mode = Mode::Normal;
        let mut last = Feed::Ignored;
        for key in seq {
            last = e.feed(*key, mode);
            // Track the handful of mode transitions dot-repeat capture depends on.
            match &last {
                Feed::Cmd(Command::EnterInsert)
                | Feed::Cmd(Command::EnterInsertAfter)
                | Feed::Cmd(Command::InsertLineStart)
                | Feed::Cmd(Command::AppendLineEnd)
                | Feed::Cmd(Command::OpenBelow)
                | Feed::Cmd(Command::OpenAbove)
                | Feed::Cmd(Command::Change(..)) => mode = Mode::Insert,
                Feed::Cmd(Command::EnterNormal) => mode = Mode::Normal,
                _ => {}
            }
        }
        (e, last)
    }

    #[test]
    fn dot_with_no_prior_change_is_a_clean_noop() {
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('.'), Mode::Normal), Feed::Ignored);
        assert!(e.last_change.is_none());
    }

    #[test]
    fn operator_change_replays_the_command_at_the_new_cursor() {
        // `dw` records Delete(1, WordFwd); `.` replays exactly that (motion re-run at the new cursor).
        let (_, last) = feed_modes(&[k('d'), k('w'), k('.')]);
        assert_eq!(
            last,
            Feed::Replay(vec![Command::Delete(1, Motion::WordFwd)])
        );
    }

    #[test]
    fn dot_does_not_overwrite_the_record_so_dot_dot_repeats() {
        // `dw..` — the second `.` replays the SAME recorded change, not "repeat of a repeat".
        let (_, last) = feed_modes(&[k('d'), k('w'), k('.'), k('.')]);
        assert_eq!(
            last,
            Feed::Replay(vec![Command::Delete(1, Motion::WordFwd)])
        );
    }

    #[test]
    fn counted_operator_is_recorded_with_its_count() {
        // `d2w` -> Delete(2, WordFwd); `.` repeats with the same count.
        let (_, last) = feed_modes(&[k('d'), k('2'), k('w'), k('.')]);
        assert_eq!(
            last,
            Feed::Replay(vec![Command::Delete(2, Motion::WordFwd)])
        );
    }

    #[test]
    fn single_key_edits_are_dot_repeatable() {
        assert_eq!(
            feed_modes(&[k('x'), k('.')]).1,
            Feed::Replay(vec![Command::DeleteUnder(1)])
        );
        assert_eq!(
            feed_modes(&[k('3'), k('x'), k('.')]).1,
            Feed::Replay(vec![Command::DeleteUnder(3)])
        );
        assert_eq!(
            feed_modes(&[k('>'), k('>'), k('.')]).1,
            Feed::Replay(vec![Command::ShiftRight(1)])
        );
        assert_eq!(
            feed_modes(&[k('d'), k('d'), k('.')]).1,
            Feed::Replay(vec![Command::Delete(1, Motion::Line)])
        );
    }

    #[test]
    fn n_dot_overrides_the_recorded_count() {
        // `3.` after `dw` replays with count 3 (Vim replaces, not multiplies).
        let (_, last) = feed_modes(&[k('d'), k('w'), k('3'), k('.')]);
        assert_eq!(
            last,
            Feed::Replay(vec![Command::Delete(3, Motion::WordFwd)])
        );
        // `2.` after `3x` replaces the 3 with 2.
        let (_, last) = feed_modes(&[k('3'), k('x'), k('2'), k('.')]);
        assert_eq!(last, Feed::Replay(vec![Command::DeleteUnder(2)]));
    }

    #[test]
    fn insert_change_replays_command_and_captured_text() {
        // `ciwFOO<Esc>` -> Change(1, InnerWord) + the inserted chars + the terminating EnterNormal.
        let (_, last) = feed_modes(&[
            k('c'),
            k('i'),
            k('w'),
            k('F'),
            k('O'),
            k('O'),
            esc(),
            k('.'),
        ]);
        assert_eq!(
            last,
            Feed::Replay(vec![
                Command::Change(1, Motion::InnerWord),
                Command::InsertChar('F'),
                Command::InsertChar('O'),
                Command::InsertChar('O'),
                Command::EnterNormal,
            ])
        );
    }

    #[test]
    fn append_insert_is_dot_repeatable_including_text() {
        // `A;<Esc>` then `.` replays AppendLineEnd + the ';' + Esc.
        let (_, last) = feed_modes(&[k('A'), k(';'), esc(), k('.')]);
        assert_eq!(
            last,
            Feed::Replay(vec![
                Command::AppendLineEnd,
                Command::InsertChar(';'),
                Command::EnterNormal,
            ])
        );
    }

    #[test]
    fn yank_is_not_dot_repeatable() {
        // Vim: `yw` is NOT a change; a following `.` has nothing to repeat.
        let mut e = InputEngine::new();
        e.feed(k('y'), Mode::Normal);
        e.feed(k('w'), Mode::Normal);
        assert!(e.last_change.is_none());
        assert_eq!(e.feed(k('.'), Mode::Normal), Feed::Ignored);
    }

    #[test]
    fn named_register_change_replays_with_its_register() {
        // `"ax` records DeleteUnder(1) carrying register a; `.` replays SetRegister(a) THEN the delete.
        let (_, last) = feed_modes(&[k('"'), k('a'), k('x'), k('.')]);
        assert_eq!(
            last,
            Feed::Replay(vec![
                Command::SetRegister(Some('a')),
                Command::DeleteUnder(1),
            ])
        );
    }

    #[test]
    fn unregistered_change_replays_without_a_register() {
        // A plain `x` after a stray-then-consumed register still replays bare (no leading SetRegister).
        let (_, last) = feed_modes(&[k('x'), k('.')]);
        assert_eq!(last, Feed::Replay(vec![Command::DeleteUnder(1)]));
    }

    #[test]
    fn motions_between_changes_do_not_clobber_the_record() {
        // `x` records; then a pure motion `w`; `.` still repeats the `x`.
        let (_, last) = feed_modes(&[k('x'), k('w'), k('.')]);
        assert_eq!(last, Feed::Replay(vec![Command::DeleteUnder(1)]));
    }
}

#[cfg(test)]
mod textobj_tests {
    use super::tests::*;
    use crate::input::*;

    fn pair(open: char, close: char, around: bool) -> Motion {
        Motion::Pair {
            open,
            close,
            around,
        }
    }

    #[test]
    fn text_objects_compose() {
        assert_eq!(
            feed("diw"),
            Feed::Cmd(Command::Delete(1, Motion::InnerWord))
        );
        assert_eq!(
            feed("ciw"),
            Feed::Cmd(Command::Change(1, Motion::InnerWord))
        );
        assert_eq!(feed("daw"), Feed::Cmd(Command::Delete(1, Motion::AWord)));
    }

    #[test]
    fn word_and_bigword_objects() {
        assert_eq!(
            feed("diW"),
            Feed::Cmd(Command::Delete(1, Motion::InnerBigWord))
        );
        assert_eq!(feed("daW"), Feed::Cmd(Command::Delete(1, Motion::ABigWord)));
    }

    #[test]
    fn paragraph_and_sentence_objects() {
        assert_eq!(
            feed("yip"),
            Feed::Cmd(Command::Yank(1, Motion::InnerParagraph))
        );
        assert_eq!(
            feed("dap"),
            Feed::Cmd(Command::Delete(1, Motion::AParagraph))
        );
        assert_eq!(
            feed("dis"),
            Feed::Cmd(Command::Delete(1, Motion::InnerSentence))
        );
        assert_eq!(
            feed("das"),
            Feed::Cmd(Command::Delete(1, Motion::ASentence))
        );
    }

    #[test]
    fn delimiter_pair_objects_and_aliases() {
        // Inner vs around.
        assert_eq!(
            feed("di("),
            Feed::Cmd(Command::Delete(1, pair('(', ')', false)))
        );
        assert_eq!(
            feed("da("),
            Feed::Cmd(Command::Delete(1, pair('(', ')', true)))
        );
        // Closer and `b` alias to the same `()` object as the opener.
        assert_eq!(
            feed("di)"),
            Feed::Cmd(Command::Delete(1, pair('(', ')', false)))
        );
        assert_eq!(
            feed("dab"),
            Feed::Cmd(Command::Delete(1, pair('(', ')', true)))
        );
        // Braces: `{`/`}`/`B` collapse.
        assert_eq!(
            feed("ci{"),
            Feed::Cmd(Command::Change(1, pair('{', '}', false)))
        );
        assert_eq!(
            feed("daB"),
            Feed::Cmd(Command::Delete(1, pair('{', '}', true)))
        );
        // Brackets and angles.
        assert_eq!(
            feed("di["),
            Feed::Cmd(Command::Delete(1, pair('[', ']', false)))
        );
        assert_eq!(
            feed("da]"),
            Feed::Cmd(Command::Delete(1, pair('[', ']', true)))
        );
        assert_eq!(
            feed("di<"),
            Feed::Cmd(Command::Delete(1, pair('<', '>', false)))
        );
    }

    #[test]
    fn quote_objects() {
        assert_eq!(
            feed("da\""),
            Feed::Cmd(Command::Delete(
                1,
                Motion::Quote {
                    ch: '"',
                    around: true
                }
            ))
        );
        assert_eq!(
            feed("ci'"),
            Feed::Cmd(Command::Change(
                1,
                Motion::Quote {
                    ch: '\'',
                    around: false
                }
            ))
        );
        assert_eq!(
            feed("yi`"),
            Feed::Cmd(Command::Yank(
                1,
                Motion::Quote {
                    ch: '`',
                    around: false
                }
            ))
        );
    }

    #[test]
    fn text_object_extends_a_visual_selection() {
        // In Visual, `i`/`a` begin a text object; it completes as a bare `Move` the core turns into a span.
        let mut e = InputEngine::new();
        let vis = Mode::Visual {
            kind: SelectKind::Charwise,
        };
        assert_eq!(e.feed(k('i'), vis), Feed::Pending);
        assert_eq!(
            e.feed(k('w'), vis),
            Feed::Cmd(Command::Move(1, Motion::InnerWord))
        );
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('i'), vis), Feed::Pending);
        assert_eq!(
            e.feed(k('('), vis),
            Feed::Cmd(Command::Move(1, pair('(', ')', false)))
        );
    }

    #[test]
    fn tag_objects_are_deferred_and_abort_cleanly() {
        // `it`/`at` are carved out (no core syntax tree). The pending object aborts to a no-op, never panics.
        assert_eq!(feed("dit"), Feed::Ignored);
        assert_eq!(feed("dat"), Feed::Ignored);
        let mut e = InputEngine::new();
        assert_eq!(
            e.feed(k('v'), Mode::Normal),
            Feed::Cmd(Command::EnterVisual {
                kind: SelectKind::Charwise
            })
        );
        assert_eq!(
            e.feed(
                k('i'),
                Mode::Visual {
                    kind: SelectKind::Charwise
                }
            ),
            Feed::Pending
        );
        assert_eq!(
            e.feed(
                k('t'),
                Mode::Visual {
                    kind: SelectKind::Charwise
                }
            ),
            Feed::Ignored
        );
    }

    #[test]
    fn bare_i_still_enters_insert() {
        let mut e = InputEngine::new();
        assert_eq!(
            e.feed(k('i'), Mode::Normal),
            Feed::Cmd(Command::EnterInsert)
        );
    }
}

#[cfg(test)]
mod search_tests {
    use super::tests::k;
    use crate::input::*;

    #[test]
    fn slash_opens_search_and_n_repeats() {
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('n'), Mode::Normal), Feed::Ignored); // no prior search yet
        assert_eq!(e.feed(k('/'), Mode::Normal), Feed::Pending);
        // Submitting the pattern yields a bare-move Search AND records it for `n`/`N`.
        assert_eq!(
            e.submit_search("foo".into()),
            Feed::Cmd(Command::Search {
                op: SearchOp::Move,
                count: 1,
                pattern: "foo".into()
            })
        );
        assert_eq!(
            e.feed(k('n'), Mode::Normal),
            Feed::Cmd(Command::SearchNext("foo".into()))
        );
        assert_eq!(
            e.feed(k('N'), Mode::Normal),
            Feed::Cmd(Command::SearchPrev("foo".into()))
        );
    }

    #[test]
    fn empty_search_pattern_is_inert() {
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('/'), Mode::Normal), Feed::Pending);
        assert_eq!(e.submit_search(String::new()), Feed::Ignored);
    }

    #[test]
    fn slash_clears_the_transient_axes_but_folds_the_operator_into_the_search() {
        // `/` is a MOTION: the transient count/op/awaiting axes are cleared (so nothing leaks into the
        // minibuffer), but an armed operator/count is CAPTURED for the search to consume — `d/pat`,
        // `2/pat`. This is the fix for the old behaviour that dropped the operator on `/`.
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('d'), Mode::Normal), Feed::Pending);
        assert_eq!(e.feed(k('/'), Mode::Normal), Feed::Pending);
        assert!(
            e.normal.op.is_none() && e.normal.awaiting == Awaiting::Nothing && e.normal.count == 0
        );
        assert_eq!(
            e.submit_search("bar".into()),
            Feed::Cmd(Command::Search {
                op: SearchOp::Delete,
                count: 1,
                pattern: "bar".into()
            })
        );
    }

    #[test]
    fn count_before_slash_selects_the_nth_match() {
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('2'), Mode::Normal), Feed::Pending);
        assert_eq!(e.feed(k('/'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.submit_search("foo".into()),
            Feed::Cmd(Command::Search {
                op: SearchOp::Move,
                count: 2,
                pattern: "foo".into()
            })
        );
    }
}

/// Property tests over the input state machine: the hierarchy is explicit, so verify it has no holes —
/// no key sequence leaks a partial command, `awaiting` and the operator axis stay consistent, and `feed`
/// is deterministic. This is the mechanical guard against the "ad-hoc resolution order" class of bug.
#[cfg(test)]
mod state_machine_props {
    use crate::input::*;
    use proptest::prelude::*;

    /// A key drawn from the meaningful command alphabet, plus arbitrary chars (find targets) and specials.
    fn any_key() -> impl Strategy<Value = KeyEvent> {
        let named = "0123456789hjklwbeWBEdcyiaoOAIxfFtT;,vVpPunN$/:gGrJ~%(){}[]<>\"'`sp"
            .chars()
            .collect::<Vec<_>>();
        prop_oneof![
            proptest::sample::select(named)
                .prop_map(|c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)),
            any::<char>().prop_map(|c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)),
            Just(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            Just(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL)),
            Just(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Just(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Just(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
        ]
    }

    fn any_mode() -> impl Strategy<Value = Mode> {
        prop_oneof![
            Just(Mode::Normal),
            Just(Mode::Insert),
            Just(Mode::Replace),
            Just(Mode::Visual {
                kind: SelectKind::Charwise
            }),
            Just(Mode::Visual {
                kind: SelectKind::Linewise
            }),
            Just(Mode::Select {
                kind: SelectKind::Charwise
            }),
            Just(Mode::Select {
                kind: SelectKind::Linewise
            }),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// No key sequence ever leaves a partial command dangling: after any outcome that is not
        /// `Feed::Pending`, every transient axis (count, operator, key-expectation) is cleared. And a
        /// text-object expectation is only ever armed with an operator present. (Never panics — implicit.)
        #[test]
        fn no_pending_state_ever_leaks(steps in prop::collection::vec((any_key(), any_mode()), 0..80)) {
            let mut e = InputEngine::new();
            for (key, mode) in steps {
                let feed = e.feed(key, mode);
                // Orthogonal-axis invariant: a text object is only ever awaited with an operator armed
                // (`diw`) or from a selection mode (`viw`) — never bare in Normal. `mode` is the mode the
                // arming key was fed with, so this checks the arm site directly.
                if let Awaiting::TextObjectChar { .. } = e.normal.awaiting {
                    let in_selection = matches!(mode, Mode::Visual { .. } | Mode::Select { .. });
                    prop_assert!(
                        e.normal.op.is_some() || in_selection,
                        "text object awaited with neither an operator nor a selection"
                    );
                }
                if feed == Feed::Pending {
                    // A pending outcome must correspond to real accumulated state — including the two
                    // Insert-only prefixes (`CTRL-O` one-shot, `CTRL-G u`), which are pending too.
                    let has_state = e.normal.count > 0
                        || e.normal.op.is_some()
                        || e.normal.awaiting != Awaiting::Nothing
                        || !e.activations.is_empty()
                        || e.insert.ctrl_g
                        || e.cmdline.is_some(); // an open command-line namespace is real pending state (F-026)
                    prop_assert!(has_state, "Feed::Pending but the engine is idle");
                } else {
                    prop_assert_eq!(e.normal.count, 0, "count leaked after {:?}", feed);
                    prop_assert!(e.normal.op.is_none(), "operator leaked after {:?}", feed);
                    prop_assert!(e.normal.awaiting == Awaiting::Nothing, "key-expectation leaked after {:?}", feed);
                    prop_assert!(e.activations.is_empty(), "one-shot leaked after {:?}", feed);
                    prop_assert!(!e.insert.ctrl_g, "ctrl-g prefix leaked after {:?}", feed);
                }
            }
        }

        /// `feed` is a pure function of (state, key, mode): two engines fed the same sequence agree at
        /// every step. Determinism is what makes trace replay sound.
        #[test]
        fn feed_is_deterministic(steps in prop::collection::vec((any_key(), any_mode()), 0..40)) {
            let mut a = InputEngine::new();
            let mut b = InputEngine::new();
            for (key, mode) in steps {
                prop_assert_eq!(a.feed(key, mode), b.feed(key, mode));
            }
        }
    }
}

#[cfg(test)]
mod cmdline_tests {
    use super::tests::k;
    use crate::input::*;

    fn special(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn enter() -> KeyEvent {
        special(KeyCode::Enter)
    }

    #[test]
    fn colon_opens_the_namespace_the_engine_owns_the_line_and_cr_executes() {
        // F-026 #1/#2: `:` enters the namespace; the engine (not the UI) owns the buffer; <CR> runs it.
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k(':'), Mode::Normal), Feed::Pending);
        assert_eq!(e.cmdline(), Some((':', "", 0)));
        assert_eq!(e.feed(k('w'), Mode::Normal), Feed::Pending);
        assert_eq!(e.feed(k('q'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.cmdline(),
            Some((':', "wq", 2)),
            "the namespace owns the line buffer + cursor"
        );
        assert_eq!(e.feed(enter(), Mode::Normal), Feed::ExecuteEx("wq".into()));
        assert_eq!(e.cmdline(), None, "<CR> closes the one-shot command line");
    }

    #[test]
    fn slash_search_now_flows_entirely_through_the_engine() {
        // F-026: no ad-hoc UI buffer — `/`, the pattern chars, and <CR> all go through feed().
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('/'), Mode::Normal), Feed::Pending);
        for c in "foo".chars() {
            assert_eq!(e.feed(k(c), Mode::Normal), Feed::Pending);
        }
        assert_eq!(
            e.feed(enter(), Mode::Normal),
            Feed::Cmd(Command::Search {
                op: SearchOp::Move,
                count: 1,
                pattern: "foo".into()
            })
        );
        assert_eq!(e.cmdline(), None);
        // And the operator-fold still works because the buffer moved INTO the engine: `d/bar`.
        assert_eq!(e.feed(k('d'), Mode::Normal), Feed::Pending);
        assert_eq!(e.feed(k('/'), Mode::Normal), Feed::Pending);
        for c in "bar".chars() {
            e.feed(k(c), Mode::Normal);
        }
        assert_eq!(
            e.feed(enter(), Mode::Normal),
            Feed::Cmd(Command::Search {
                op: SearchOp::Delete,
                count: 1,
                pattern: "bar".into()
            })
        );
    }

    #[test]
    fn esc_aborts_the_command_line_without_executing() {
        let mut e = InputEngine::new();
        e.feed(k(':'), Mode::Normal);
        e.feed(k('x'), Mode::Normal);
        assert_eq!(e.feed(special(KeyCode::Esc), Mode::Normal), Feed::Ignored);
        assert_eq!(e.cmdline(), None, "<Esc> closes the line and runs nothing");
    }

    #[test]
    fn backspace_deletes_back_in_the_owned_buffer() {
        let mut e = InputEngine::new();
        e.feed(k(':'), Mode::Normal);
        e.feed(k('a'), Mode::Normal);
        e.feed(k('b'), Mode::Normal);
        e.feed(special(KeyCode::Backspace), Mode::Normal);
        assert_eq!(e.cmdline(), Some((':', "a", 1)));
    }

    #[test]
    fn gq_enters_ex_mode_reprompts_and_visual_exits() {
        // F-026 #3: `gQ` (only) enters Ex mode; <CR> runs AND re-opens; `:visual` leaves it.
        let mut e = InputEngine::new();
        e.feed(k('g'), Mode::Normal);
        assert_eq!(e.feed(k('Q'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.cmdline(),
            Some((':', "", 0)),
            "gQ opens the Ex command line"
        );
        for c in "set".chars() {
            e.feed(k(c), Mode::Normal);
        }
        assert_eq!(e.feed(enter(), Mode::Normal), Feed::ExecuteEx("set".into()));
        assert_eq!(
            e.cmdline(),
            Some((':', "", 0)),
            "Ex mode re-prompts after <CR>"
        );
        for c in "visual".chars() {
            e.feed(k(c), Mode::Normal);
        }
        assert_eq!(e.feed(enter(), Mode::Normal), Feed::Ignored);
        assert_eq!(e.cmdline(), None, "`:visual` exits Ex mode");
    }

    #[test]
    fn bare_q_is_not_an_ex_mode_key() {
        // At the pinned Neovim revision `Q` is replay-last-register, NOT Ex mode — only `gQ` opens it.
        let mut e = InputEngine::new();
        let out = e.feed(k('Q'), Mode::Normal);
        assert_ne!(
            out,
            Feed::Pending,
            "a bare Q must not open the Ex command line"
        );
        assert_eq!(e.cmdline(), None);
    }
}

#[cfg(test)]
mod namespace_tests {
    use super::tests::k;
    use crate::input::*;
    use ruse_core::keymap::UnmatchedKey;

    /// F-003 #3 (VS-OBL-4 / KL-OBL-1): each of the eight Vim map-mode namespaces is addressable in its
    /// own right, and F-003 #1 (KL-OBL-3): the Vim profile is depth-1 and SEALED — declared, not an
    /// accident. Asserted here rather than left as a comment so the property can never silently rot.
    #[test]
    fn all_eight_namespaces_are_addressable_depth_one_and_sealed() {
        let profile = VimProfile::new();
        for ns in VimProfile::all() {
            let stack = profile.stack(ns);
            assert_eq!(
                stack.depth(),
                1,
                "{ns:?} must be a single sealed layer (depth 1)"
            );
            let layer = stack
                .layer(ns.id())
                .expect("the namespace names its own layer");
            assert_eq!(
                layer.id(),
                ns.id(),
                "the layer is addressable by its namespace id"
            );
            assert!(layer.is_sealed(), "{ns:?} must be sealed (KL-OBL-3)");
        }
    }

    /// F-003 #4 (KL-OBL-2): every namespace declares its census unmatched-key policy explicitly —
    /// there is no engine-wide default. The values are vim-style.yaml's, derived from `map_mode`.
    #[test]
    fn each_namespace_declares_its_census_policy() {
        let profile = VimProfile::new();
        let expect = [
            (Ns::Normal, UnmatchedKey::Ignore),
            (Ns::OperatorPending, UnmatchedKey::Abort),
            (Ns::Insert, UnmatchedKey::Insert),
            (Ns::Cmdline, UnmatchedKey::Append),
            (Ns::Visual, UnmatchedKey::Ignore),
            (Ns::Select, UnmatchedKey::ReplaceSelection),
            (Ns::Terminal, UnmatchedKey::Forward),
            (Ns::Lang, UnmatchedKey::Translate),
        ];
        for (ns, policy) in expect {
            let layer = profile.stack(ns).layer(ns.id()).expect("layer exists");
            assert_eq!(layer.unmatched(), policy, "{ns:?} policy");
        }
    }

    /// F-003 #4: the five OPEN policies (insert/append/overwrite/replace-selection/forward) are all
    /// present across the declared namespaces and are distinct from the two CLOSED ones — the axis a
    /// shared `Feed::Ignored` fallthrough erases. `overwrite` rides the Replace namespace (insert
    /// family), the rest ride the eight.
    #[test]
    fn the_five_open_policies_are_declared_and_distinct() {
        let profile = VimProfile::new();
        let policy = |ns: Ns| profile.stack(ns).layer(ns.id()).unwrap().unmatched();
        let open = [
            policy(Ns::Insert),
            policy(Ns::Cmdline),
            policy(Ns::Replace),
            policy(Ns::Select),
            policy(Ns::Terminal),
        ];
        assert_eq!(
            open,
            [
                UnmatchedKey::Insert,
                UnmatchedKey::Append,
                UnmatchedKey::Overwrite,
                UnmatchedKey::ReplaceSelection,
                UnmatchedKey::Forward,
            ],
            "all five open policies are declared, each on a distinct namespace"
        );
        for p in open {
            assert!(p.is_open(), "{p:?} is an open policy");
        }
        assert!(!policy(Ns::Normal).is_open(), "Normal is closed/ignore");
        assert!(
            !policy(Ns::OperatorPending).is_open(),
            "Opr is closed/abort"
        );
    }

    /// F-003 #4: the `open/overwrite` policy is EXERCISED through the router — a printable key nothing
    /// binds in Replace mode overwrites (via the Replace layer's declared policy), tab-aware in gR.
    #[test]
    fn overwrite_policy_is_exercised_through_the_router() {
        let mut e = InputEngine::new();
        assert_eq!(
            e.feed(k('x'), Mode::Replace),
            Feed::Cmd(Command::ReplaceType('x')),
            "R + printable overwrites (open/overwrite)"
        );
        assert_eq!(
            e.feed(k('y'), Mode::VirtualReplace),
            Feed::Cmd(Command::VirtualReplaceType('y')),
            "gR + printable overwrites tab-aware"
        );
        // The bound keys still resolve through the same layer.
        assert_eq!(
            e.feed(
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                Mode::Replace
            ),
            Feed::Cmd(Command::EnterNormal),
        );
    }

    /// F-003 #4: the `open/insert` and `open/replace-selection` policies are exercised — an unmatched
    /// printable does the namespace's OPEN thing, never falls through to a shared ignore.
    #[test]
    fn insert_and_replace_selection_policies_are_exercised() {
        let mut e = InputEngine::new();
        assert_eq!(
            e.feed(k('z'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('z')),
            "Insert: printable is text (open/insert)"
        );
        let mut e2 = InputEngine::new();
        assert_eq!(
            e2.feed(
                k('q'),
                Mode::Select {
                    kind: SelectKind::Charwise
                }
            ),
            Feed::Cmd(Command::ReplaceSelection('q')),
            "Select: printable replaces the selection (open/replace-selection)"
        );
    }
}

#[cfg(test)]
mod layer_state_tests {
    use super::tests::k;
    use crate::input::*;

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    /// F-003 #5 (KL-OBL-4): the Normal-family grammar layer OWNS its count/operator/awaiting, and that
    /// state DIES when the layer deactivates — it is not carried into another namespace, and it is the
    /// layer's lifecycle that clears it (dropped as `NormalState::default`), not the engine resetting
    /// named fields on a foreign key.
    #[test]
    fn normal_grammar_state_dies_when_the_layer_deactivates() {
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('2'), Mode::Normal), Feed::Pending);
        assert_eq!(e.normal.count, 2, "the Normal layer owns the pending count");
        // `d` consumes the count into the armed operator (Vim moves 2 into `2d`), which the layer
        // still owns — the point is that all of it is Normal-family state.
        assert_eq!(e.feed(k('d'), Mode::Normal), Feed::Pending);
        assert!(
            e.normal.op.is_some(),
            "the Normal layer owns the armed operator"
        );
        // A key arriving in Insert means the Normal family deactivated: its state dies with it.
        let _ = e.feed(k('x'), Mode::Insert);
        assert_eq!(e.normal.count, 0, "count died with the Normal layer");
        assert!(e.normal.op.is_none(), "operator died with the Normal layer");
        assert!(
            e.normal.awaiting == Awaiting::Nothing,
            "key-expectation died with the Normal layer"
        );
    }

    /// F-003 #5/#6: an `i_CTRL-O` one-shot is a return address on the activation stack (KL-OBL-5); a
    /// key arriving back in Normal abandons the pending Insert resume rather than leaking it.
    #[test]
    fn insert_one_shot_activation_is_abandoned_when_insert_deactivates() {
        let mut e = InputEngine::new();
        assert_eq!(e.feed(ctrl('o'), Mode::Insert), Feed::Pending);
        assert!(
            !e.activations.is_empty(),
            "CTRL-O pushed a one-shot activation"
        );
        // The next key arrives back in Normal — the Insert resume address no longer applies.
        let _ = e.feed(k('l'), Mode::Normal);
        assert!(
            e.activations.is_empty(),
            "the pending Insert one-shot was abandoned"
        );
    }

    /// KL-OBL-4 must NOT drop the Normal state during an `i_CTRL-O` one-shot: the one-shot runs a
    /// single Normal command from WITHIN Insert, so the Normal family is momentarily active there and
    /// its count must survive the mode still reading `Insert` (`i<C-o>2l` moves two).
    #[test]
    fn one_shot_keeps_normal_state_alive_inside_insert() {
        let mut e = InputEngine::new();
        assert_eq!(e.feed(ctrl('o'), Mode::Insert), Feed::Pending);
        // Build a count through the Normal grammar while the mode is still Insert.
        assert_eq!(e.feed(k('2'), Mode::Insert), Feed::Pending);
        assert_eq!(
            e.normal.count, 2,
            "the one-shot keeps the Normal count alive inside Insert (not dropped by KL-OBL-4)"
        );
    }

    /// F-003 #6 (KL-OBL-5): `i_CTRL-O` records a return ADDRESS (`resume: Insert`) on the activation
    /// stack — WHENCE control came, which a flat boolean edge cannot express — and the completing
    /// command pops it, resuming Insert. `t_CTRL-\ CTRL-O` is the SAME construct (`resume: Terminal`),
    /// deferred only because no terminal buffers exist yet.
    #[test]
    fn one_shot_records_a_return_address_and_pops_it_on_completion() {
        let mut e = InputEngine::new();
        assert_eq!(e.feed(ctrl('o'), Mode::Insert), Feed::Pending);
        assert_eq!(
            e.activations,
            vec![Suspended { resume: Ns::Insert }],
            "the return address is Insert — whence the one-shot came"
        );
        // One Normal command (`l` = move right) completes the one-shot; its return address pops.
        let out = e.feed(k('l'), Mode::Insert);
        assert!(
            matches!(out, Feed::Cmd(_)),
            "the borrowed Normal command ran"
        );
        assert!(
            e.activations.is_empty(),
            "completion popped the return address, resuming Insert"
        );
    }

    /// F-003 #7 (VS-OBL-1): mode is per-window/per-buffer — the same key resolves through DIFFERENT
    /// namespaces depending on the mode the engine is fed (F-007 gives each View its own mode, and the
    /// frontend feeds the focused View's mode). Here the same engine dispatches one window's Insert
    /// keystroke as text and another window's Normal keystroke as a command.
    #[test]
    fn the_same_key_resolves_by_the_per_window_mode() {
        let mut e = InputEngine::new();
        // Window A is in Insert: `x` is literal text (open/insert).
        assert_eq!(
            e.feed(k('x'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('x'))
        );
        // Window B (same engine, next keystroke) is in Normal: `x` is delete-char, NOT text.
        let out = e.feed(k('x'), Mode::Normal);
        assert_ne!(
            out,
            Feed::Cmd(Command::InsertChar('x')),
            "in Normal the same key is a command, not inserted text"
        );
    }
}

#[cfg(test)]
mod substitute_parse_tests {
    use crate::input::*;

    fn sub(line: &str) -> SubSpec {
        match parse_ex(line) {
            Ex::Substitute(s) => s,
            other => panic!("expected Substitute, got {other:?}"),
        }
    }

    #[test]
    fn plain_substitute_current_line() {
        let s = sub("s/foo/bar/");
        assert_eq!(s.range, SubRange::CurrentLine);
        assert_eq!(s.pattern, "foo");
        assert_eq!(s.replacement, "bar");
        assert!(!s.global && s.ignore_case.is_none() && !s.confirm);
    }

    #[test]
    fn whole_file_global() {
        let s = sub("%s/a/b/g");
        assert_eq!(s.range, SubRange::WholeFile);
        assert!(s.global);
    }

    #[test]
    fn line_range_and_flags() {
        let s = sub("2,5s/a/b/gi");
        assert_eq!(s.range, SubRange::Lines(2, 5));
        assert!(s.global);
        assert_eq!(s.ignore_case, Some(true));
    }

    #[test]
    fn capital_i_forces_case_sensitive_and_c_is_confirm() {
        let s = sub("s/a/b/Ic");
        assert_eq!(s.ignore_case, Some(false));
        assert!(s.confirm);
    }

    #[test]
    fn gdefault_inverts_the_g_flag() {
        // With gdefault ON, a BARE `:s///` is global and `:s///g` is single.
        assert!(parse_substitute("s/a/b/", true).unwrap().global);
        assert!(!parse_substitute("s/a/b/g", true).unwrap().global);
    }

    #[test]
    fn escaped_delimiter_stays_in_the_pattern() {
        let s = sub("s/a\\/b/c/");
        assert_eq!(s.pattern, "a\\/b");
        assert_eq!(s.replacement, "c");
    }

    #[test]
    fn sort_is_not_a_substitute() {
        assert!(matches!(parse_ex("sort"), Ex::Unknown(_)));
    }
}

#[cfg(test)]
mod global_parse_tests {
    use crate::input::*;

    fn glob(line: &str) -> GlobalSpec {
        match parse_ex(line) {
            Ex::Global(g) => g,
            other => panic!("expected Global, got {other:?}"),
        }
    }

    #[test]
    fn global_delete_defaults_to_whole_file() {
        let g = glob("g/foo/d");
        assert_eq!(g.range, SubRange::WholeFile);
        assert_eq!(g.pattern, "foo");
        assert!(!g.negate);
        assert_eq!(g.cmd, GlobalCmd::Delete);
    }

    #[test]
    fn bang_and_v_negate() {
        assert!(glob("g!/foo/d").negate);
        assert!(glob("v/foo/d").negate);
        assert!(glob("vglobal/foo/d").negate);
    }

    #[test]
    fn global_substitute_subcommand() {
        let g = glob("g/foo/s/x/y/g");
        assert_eq!(g.pattern, "foo");
        assert_eq!(
            g.cmd,
            GlobalCmd::Substitute {
                pattern: "x".into(),
                replacement: "y".into(),
                flags: SubFlags {
                    global: true,
                    ignore_case: None
                },
            }
        );
    }

    #[test]
    fn ranged_global() {
        let g = glob("2,5g/foo/d");
        assert_eq!(g.range, SubRange::Lines(2, 5));
    }

    #[test]
    fn vsplit_is_not_a_global() {
        // `:vsplit` must stay the window command, not be parsed as `:v`-global.
        assert_eq!(parse_ex("vsplit"), Ex::VSplit);
        assert_eq!(parse_ex("vs"), Ex::VSplit);
    }
}

#[cfg(test)]
mod nohighlight_parse_tests {
    use crate::input::*;

    #[test]
    fn noh_variants_parse() {
        assert_eq!(parse_ex("noh"), Ex::NoHighlight);
        assert_eq!(parse_ex("nohl"), Ex::NoHighlight);
        assert_eq!(parse_ex("nohlsearch"), Ex::NoHighlight);
    }
}

#[cfg(test)]
mod binding_label_tests {
    use crate::input::*;

    /// F-004 #2: a command with a STATIC layer binding reports it; a grammar/ex command (no layer
    /// binding) reports None — the deliberate MVP scope (static layer bindings only).
    #[test]
    fn binding_label_reports_static_layer_bindings_only() {
        let e = InputEngine::new();
        // The Insert layer binds Esc -> EnterNormal, Enter -> InsertNewline, Backspace -> DeleteBack.
        assert_eq!(
            e.binding_label(&Command::EnterNormal).as_deref(),
            Some("Esc")
        );
        assert_eq!(
            e.binding_label(&Command::InsertNewline).as_deref(),
            Some("CR")
        );
        assert_eq!(e.binding_label(&Command::DeleteBack).as_deref(), Some("BS"));
        // Undo (`u`) is Normal grammar, not a layer binding, so it has no static binding here.
        assert_eq!(e.binding_label(&Command::Undo), None);
        assert_eq!(e.binding_label(&Command::Save), None);
    }
}
