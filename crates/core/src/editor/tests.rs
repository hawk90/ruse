#[cfg(test)]
mod register_tests {
    use crate::editor::*;

    fn run(initial: &str, cmds: &[Command]) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        for c in cmds {
            apply_command(&mut st, c);
        }
        st
    }

    fn text(st: &EditorState) -> String {
        String::from_utf8(st.bytes().to_vec()).expect("utf8")
    }

    #[test]
    fn set_cursor_clamps_out_of_range_and_mid_codepoint() {
        // Past the buffer end → clamped to the end (no panic in the following col_of/slice).
        let mut st = EditorState::new(b"abc".to_vec());
        st.set_cursor(999);
        assert_eq!(st.cursor(), 3);

        // Mid-codepoint → snapped back to the char boundary (`é` is 2 bytes).
        let mut st = EditorState::new("é".as_bytes().to_vec());
        st.set_cursor(1);
        assert_eq!(st.cursor(), 0);
    }

    #[test]
    fn yy_then_p_duplicates_the_line_below() {
        let st = run(
            "aaa\nbbb\n",
            &[
                Command::Yank(1, Motion::Line),
                Command::Paste {
                    after: true,
                    count: 1,
                    move_after: false,
                },
            ],
        );
        assert_eq!(text(&st), "aaa\naaa\nbbb\n");
    }

    #[test]
    fn dd_fills_register_and_p_pastes_it_below() {
        let st = run(
            "one\ntwo\n",
            &[
                Command::Delete(1, Motion::Line),
                Command::Paste {
                    after: true,
                    count: 1,
                    move_after: false,
                },
            ],
        );
        assert_eq!(text(&st), "two\none\n");
    }

    #[test]
    fn bracket_p_reindents_a_linewise_paste_to_the_current_line() {
        // Yank "  copyme" (indent 2) linewise, move to "    target" (indent 4), `]p` pastes below with the
        // indent shifted by +2 so the pasted line matches the target's indent.
        let st = run(
            "    target\n  copyme\n",
            &[
                Command::Move(1, Motion::Down),
                Command::Yank(1, Motion::Line),
                Command::Move(1, Motion::Up),
                Command::PasteIndent {
                    after: true,
                    count: 1,
                },
            ],
        );
        assert_eq!(text(&st), "    target\n    copyme\n  copyme\n");
    }

    #[test]
    fn bracket_p_shifts_a_multi_line_block_by_a_constant_delta() {
        // A two-line register (indents 2 and 6) pasted onto an indent-0 line: delta = 0-2 = -2, so both
        // lines dedent by 2 → indents 0 and 4, preserving their relative structure.
        let st = run(
            "top\n  a\n      b\n",
            &[
                Command::Move(1, Motion::Down),
                Command::Yank(2, Motion::Line),
                Command::Move(2, Motion::Up),
                Command::PasteIndent {
                    after: true,
                    count: 1,
                },
            ],
        );
        assert_eq!(text(&st), "top\na\n    b\n  a\n      b\n");
    }

    #[test]
    fn bracket_capital_p_pastes_above_with_indent() {
        let st = run(
            "    target\n  copyme\n",
            &[
                Command::Move(1, Motion::Down),
                Command::Yank(1, Motion::Line),
                Command::Move(1, Motion::Up),
                Command::PasteIndent {
                    after: false,
                    count: 1,
                },
            ],
        );
        assert_eq!(text(&st), "    copyme\n    target\n  copyme\n");
    }

    #[test]
    fn bracket_p_on_a_charwise_register_pastes_unchanged() {
        // `]p` on a charwise register behaves like `p` (no indent adjust).
        let st = run(
            "abcXYZ",
            &[
                Command::Yank(3, Motion::Right), // "abc" charwise into unnamed
                Command::PasteIndent {
                    after: true,
                    count: 1,
                },
            ],
        );
        assert_eq!(text(&st), "aabcbcXYZ");
    }

    #[test]
    fn insert_ctrl_w_deletes_the_word_before_the_caret() {
        // `A<C-w>` at end of "foo bar" removes "bar", staying in Insert.
        let st = run(
            "foo bar",
            &[Command::AppendLineEnd, Command::InsertDeleteWordBack],
        );
        assert_eq!(text(&st), "foo ");
        assert_eq!(st.mode(), Mode::Insert);
        assert_eq!(st.cursor(), 4);
        // At column 0 it is a no-op.
        let st = run("hi", &[Command::EnterInsert, Command::InsertDeleteWordBack]);
        assert_eq!(text(&st), "hi");
    }

    #[test]
    fn insert_ctrl_u_deletes_to_first_non_blank_then_indent() {
        // First `<C-u>` deletes back to the first non-blank (keeps the indent); a second deletes the indent.
        let st = run(
            "    hello",
            &[Command::AppendLineEnd, Command::InsertDeleteToLineStart],
        );
        assert_eq!(text(&st), "    ", "indent preserved on the first C-u");
        assert_eq!(st.cursor(), 4);
        let st = run(
            "    hello",
            &[
                Command::AppendLineEnd,
                Command::InsertDeleteToLineStart,
                Command::InsertDeleteToLineStart,
            ],
        );
        assert_eq!(text(&st), "", "the second C-u removes the indent too");
    }

    #[test]
    fn insert_ctrl_t_and_ctrl_d_indent_the_line() {
        // Default indent is 4 spaces. `A<C-t>` indents "x" → "    x" and the caret rides right.
        let st = run("x", &[Command::AppendLineEnd, Command::InsertIndent]);
        assert_eq!(text(&st), "    x");
        assert_eq!(st.mode(), Mode::Insert);
        assert_eq!(st.cursor(), 5, "caret moved right by the inserted indent");
        // `<C-d>` on an indented line removes one shiftwidth; the caret rides left.
        let st = run("    x", &[Command::AppendLineEnd, Command::InsertDedent]);
        assert_eq!(text(&st), "x");
        assert_eq!(st.cursor(), 1);
        // `<C-d>` with no indent is a no-op.
        let st = run("x", &[Command::EnterInsert, Command::InsertDedent]);
        assert_eq!(text(&st), "x");
    }

    #[test]
    fn insert_register_splices_register_text_at_the_caret() {
        // `y3l` yanks "foo" into the unnamed register; `i` then `<C-r>"` inserts it at the caret.
        let st = run(
            "foobar",
            &[
                Command::Yank(3, Motion::Right),
                Command::EnterInsert,
                Command::InsertRegister('"'),
            ],
        );
        assert_eq!(text(&st), "foofoobar");
        assert_eq!(st.mode(), Mode::Insert, "stays in Insert after <C-r>");
        assert_eq!(st.cursor(), 3, "cursor lands after the inserted text");
    }

    #[test]
    fn insert_register_reads_a_named_slot_and_noops_when_empty() {
        // `"ay3l` fills "a; `<C-r>a` inserts it. `<C-r>z` (empty) inserts nothing.
        let st = run(
            "abcXYZ",
            &[
                Command::SetRegister(Some('a')),
                Command::Yank(3, Motion::Right),
                Command::Move(100, Motion::Right),
                Command::EnterInsertAfter,
                Command::InsertRegister('a'),
                Command::InsertRegister('z'),
            ],
        );
        assert_eq!(
            text(&st),
            "abcXYZabc",
            "named register inserted, empty one is a no-op"
        );
    }

    #[test]
    fn numbered_ring_paste_reaches_older_line_deletes() {
        // Two successive `dd`s push the numbered ring; `"2p` recalls the FIRST-deleted line, `"1p` the
        // most recent — the end-to-end proof that a delete commit feeds the ring (RegWrite::Edit).
        let st = run(
            "one\ntwo\nthree\n",
            &[
                Command::Delete(1, Motion::Line), // delete "one" → "1
                Command::Delete(1, Motion::Line), // delete "two" → "1, "one" shifts to "2
                Command::SetRegister(Some('2')),
                Command::Paste {
                    after: true,
                    count: 1,
                    move_after: false,
                },
            ],
        );
        // Buffer is "three"; pasting "2 (="one\n") below the first line.
        assert_eq!(text(&st), "three\none\n");
    }

    #[test]
    fn small_delete_register_recalls_a_sub_line_delete() {
        // `x` is a sub-line delete → the small-delete register `"-`; `"-p` puts it back.
        let st = run(
            "abc",
            &[
                Command::DeleteUnder(1), // delete 'a' → "-
                Command::SetRegister(Some('-')),
                Command::Paste {
                    after: true,
                    count: 1,
                    move_after: false,
                },
            ],
        );
        assert_eq!(text(&st), "bac");
    }

    #[test]
    fn xp_transposes_two_characters() {
        // The classic Vim idiom: `x` yanks the char, `p` puts it after the next one.
        let st = run(
            "abc",
            &[
                Command::DeleteUnder(1),
                Command::Paste {
                    after: true,
                    count: 1,
                    move_after: false,
                },
            ],
        );
        assert_eq!(text(&st), "bac");
    }

    #[test]
    fn charwise_paste_after_inserts_past_the_cursor() {
        let st = run(
            "foo",
            &[
                Command::Yank(1, Motion::Right),
                Command::Paste {
                    after: true,
                    count: 1,
                    move_after: false,
                },
            ],
        );
        assert_eq!(text(&st), "ffoo");
    }

    #[test]
    fn gp_leaves_cursor_just_after_the_pasted_text() {
        // Yank "f", `p` (move_after=false) rests ON the pasted char; `gp` rests just AFTER it.
        let p = run(
            "foo",
            &[
                Command::Yank(1, Motion::Right),
                Command::Paste {
                    after: true,
                    count: 1,
                    move_after: false,
                },
            ],
        );
        let gp = run(
            "foo",
            &[
                Command::Yank(1, Motion::Right),
                Command::Paste {
                    after: true,
                    count: 1,
                    move_after: true,
                },
            ],
        );
        assert_eq!(text(&p), "ffoo");
        assert_eq!(text(&gp), "ffoo", "same text as p");
        assert_eq!(
            gp.cursor(),
            p.cursor() + 1,
            "gp rests one past where p rested"
        );
    }

    #[test]
    fn linewise_capital_p_pastes_above() {
        // Move onto line "y", yank it, then `P` duplicates it above.
        let st = run(
            "x\ny\n",
            &[
                Command::MoveDown,
                Command::Yank(1, Motion::Line),
                Command::Paste {
                    after: false,
                    count: 1,
                    move_after: false,
                },
            ],
        );
        assert_eq!(text(&st), "x\ny\ny\n");
    }

    #[test]
    fn linewise_paste_on_last_line_without_trailing_newline() {
        // "b" has no trailing newline; the register normalizes it and paste-below adds a clean line.
        let st = run(
            "a\nb",
            &[
                Command::MoveDown,
                Command::Yank(1, Motion::Line),
                Command::Paste {
                    after: true,
                    count: 1,
                    move_after: false,
                },
            ],
        );
        assert_eq!(text(&st), "a\nb\nb");
    }

    #[test]
    fn paste_from_empty_register_is_a_noop() {
        let st = run(
            "hello",
            &[Command::Paste {
                after: true,
                count: 1,
                move_after: false,
            }],
        );
        assert_eq!(text(&st), "hello");
        assert!(st.register().is_empty());
    }

    #[test]
    fn delete_updates_the_register_geometry() {
        // A charwise delete stores charwise; a linewise delete stores linewise.
        let st = run("word\n", &[Command::Delete(1, Motion::Right)]);
        assert!(!st.register().is_linewise());
        let st = run("word\n", &[Command::Delete(1, Motion::Line)]);
        assert!(st.register().is_linewise());
    }

    #[test]
    fn count_paste_repeats_charwise_register_inline() {
        // `yl2p` on "abc": yank "a", paste it twice after the cursor -> "aaabc", cursor on the last copy.
        let st = run(
            "abc",
            &[
                Command::Yank(1, Motion::Right),
                Command::Paste {
                    after: true,
                    count: 2,
                    move_after: false,
                },
            ],
        );
        assert_eq!(text(&st), "aaabc");
        assert_eq!(st.cursor(), 2, "cursor lands on the last pasted byte");
        assert_eq!(
            st.register().text(),
            b"a",
            "the register is unchanged by paste"
        );
    }

    #[test]
    fn named_yank_writes_slot_and_mirrors_unnamed() {
        // `"ayiw` on "foo bar": yank "foo" into register a, which also mirrors the unnamed slot.
        let st = run(
            "foo bar",
            &[
                Command::SetRegister(Some('a')),
                Command::Yank(1, Motion::InnerWord),
            ],
        );
        assert_eq!(st.registers().get(Some('a')).text(), b"foo");
        assert_eq!(
            st.register().text(),
            b"foo",
            "unnamed mirrors the named write"
        );
    }

    #[test]
    fn named_paste_reads_the_named_slot() {
        // `"ayiw$"ap` on "foo bar" -> "foo barfoo" (the oracle fixture reg_named_yank_paste).
        let st = run(
            "foo bar",
            &[
                Command::SetRegister(Some('a')),
                Command::Yank(1, Motion::InnerWord),
                Command::Move(1, Motion::LineEnd),
                Command::SetRegister(Some('a')),
                Command::Paste {
                    after: true,
                    count: 1,
                    move_after: false,
                },
            ],
        );
        assert_eq!(text(&st), "foo barfoo");
        assert_eq!(st.cursor(), 9);
    }

    #[test]
    fn plain_edit_does_not_leak_into_a_named_slot() {
        // After a named yank, a plain (unregistered) delete must write ONLY the unnamed slot — the pending
        // register is one-shot and cleared once consumed.
        let st = run(
            "foo bar",
            &[
                Command::SetRegister(Some('a')),
                Command::Yank(1, Motion::InnerWord), // a = unnamed = "foo"
                Command::DeleteUnder(1),             // plain x: unnamed only
            ],
        );
        assert_eq!(
            st.registers().get(Some('a')).text(),
            b"foo",
            "named slot untouched"
        );
        assert_eq!(st.register().text(), b"f", "plain x wrote unnamed only");
    }

    #[test]
    fn uppercase_register_appends() {
        // `"ayiw` then `"Ayiw` on the next word appends charwise -> "foobar" (matches the nvim oracle).
        let st = run(
            "foo bar",
            &[
                Command::SetRegister(Some('a')),
                Command::Yank(1, Motion::InnerWord),
                Command::Move(1, Motion::WordFwd),
                Command::SetRegister(Some('A')),
                Command::Yank(1, Motion::InnerWord),
            ],
        );
        assert_eq!(st.registers().get(Some('a')).text(), b"foobar");
        assert_eq!(st.register().text(), b"foobar");
    }

    #[test]
    fn stray_register_selection_is_forgotten_by_a_motion() {
        // `"a` then a bare motion (no operator) drops the selection; a later plain delete stays unnamed-only.
        let st = run(
            "foo bar",
            &[
                Command::SetRegister(Some('a')),
                Command::Move(1, Motion::Right), // consumes+clears the pending register
                Command::DeleteUnder(1),
            ],
        );
        assert!(
            st.registers().get(Some('a')).is_empty(),
            "named slot never written"
        );
    }

    #[test]
    fn cc_preserves_indent_and_captures_linewise() {
        // `cc` (Change over Motion::Line) keeps the first line's indent, deletes the rest, enters Insert
        // at the indent end, and captures the WHOLE line linewise (indent + trailing newline included).
        let st = run("  hello\nworld", &[Command::Change(1, Motion::Line)]);
        assert_eq!(text(&st), "  \nworld", "leading indent survives cc");
        assert_eq!(st.cursor(), 2, "cursor sits at the end of the kept indent");
        assert_eq!(st.mode(), Mode::Insert);
        assert!(st.register().is_linewise());
        assert_eq!(st.register().text(), b"  hello\n");
    }
}

#[cfg(test)]
mod visual_swap_tests {
    use crate::editor::*;

    fn run(initial: &str, cmds: &[Command]) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        for c in cmds {
            apply_command(&mut st, c);
        }
        st
    }

    fn text(st: &EditorState) -> String {
        String::from_utf8(st.bytes().to_vec()).expect("utf8")
    }

    #[test]
    fn swap_then_extend_then_delete() {
        // Parity fixture visual_o_swap_then_extend on "abcde": `lll`→col3, `v` anchors col3, `h`→col2
        // (sel "c"), `o` swaps (cursor col3, anchor col2), `l`→col4 (sel "cde"), `d` deletes it → "ab".
        let st = run(
            "abcde",
            &[
                Command::Move(1, Motion::Right),
                Command::Move(1, Motion::Right),
                Command::Move(1, Motion::Right),
                Command::EnterVisual {
                    kind: SelectKind::Charwise,
                },
                Command::Move(1, Motion::Left),
                Command::SwapSelectionEnds,
                Command::Move(1, Motion::Right),
                Command::DeleteSelection,
            ],
        );
        assert_eq!(text(&st), "ab");
        assert_eq!(st.register().text(), b"cde");
        assert!(!st.register().is_linewise());
        assert_eq!(st.mode(), Mode::Normal);
    }

    #[test]
    fn replace_mode_overwrites_appends_restores_and_undoes() {
        // Overwrite: R over "hello" typing x,y → "xyllo".
        let st = run(
            "hello",
            &[
                Command::EnterReplace,
                Command::ReplaceType('x'),
                Command::ReplaceType('y'),
                Command::EnterNormal,
            ],
        );
        assert_eq!(text(&st), "xyllo");
        assert_eq!(st.mode(), Mode::Normal);

        // Append past EOL, then <BS> deletes the appended char (not a restore).
        let st = run(
            "ab",
            &[
                Command::EnterReplace,
                Command::ReplaceType('X'),
                Command::ReplaceType('Y'),
                Command::ReplaceType('Z'), // appended (past EOL)
                Command::ReplaceBackspace, // deletes the appended Z
                Command::EnterNormal,
            ],
        );
        assert_eq!(text(&st), "XY");

        // Backspace restores the overwritten originals.
        let st = run(
            "hello",
            &[
                Command::EnterReplace,
                Command::ReplaceType('x'),
                Command::ReplaceType('y'),
                Command::ReplaceBackspace,
                Command::ReplaceBackspace,
                Command::EnterNormal,
            ],
        );
        assert_eq!(text(&st), "hello", "BS restored both overwritten chars");

        // Undo of a whole R session restores the original line as one group (oracle can't observe this).
        let st = run(
            "hello",
            &[
                Command::EnterReplace,
                Command::ReplaceType('x'),
                Command::ReplaceType('y'),
                Command::ReplaceType('z'),
                Command::EnterNormal,
                Command::Undo,
            ],
        );
        assert_eq!(text(&st), "hello", "undo reverts the whole R session");
    }

    #[test]
    fn ctrl_g_u_breaks_the_undo_group_mid_insert_session() {
        // `i ab <C-g>u cd <Esc>`: BreakUndo splits the insert session into two undo groups, so the first
        // `u` reverts only "cd" (leaving "ab"), where without the break one `u` would revert the whole
        // session. Not oracle-observable (its set_lines is not an undo boundary) — hence a core test.
        let with_break = run(
            "",
            &[
                Command::EnterInsert,
                Command::InsertChar('a'),
                Command::InsertChar('b'),
                Command::BreakUndo,
                Command::InsertChar('c'),
                Command::InsertChar('d'),
                Command::EnterNormal,
                Command::Undo,
            ],
        );
        assert_eq!(
            text(&with_break),
            "ab",
            "the undo-break makes `u` stop at the CTRL-G u point"
        );
        // A second undo reverts the first group too.
        let mut st = with_break;
        apply_command(&mut st, &Command::Undo);
        assert_eq!(text(&st), "", "the second undo reverts the pre-break group");

        // Contrast: the SAME edits without the break are one group, so one `u` clears everything.
        let no_break = run(
            "",
            &[
                Command::EnterInsert,
                Command::InsertChar('a'),
                Command::InsertChar('b'),
                Command::InsertChar('c'),
                Command::InsertChar('d'),
                Command::EnterNormal,
                Command::Undo,
            ],
        );
        assert_eq!(
            text(&no_break),
            "",
            "without a break the whole insert session is one undo group"
        );
    }

    #[test]
    fn gv_reselects_the_last_visual_then_deletes_it() {
        // Parity fixture gv_reselect on "hello world": `viw` selects "hello", `y` yanks and leaves Visual,
        // `gv` re-selects the same span, `d` deletes it → " world". The selection survives the round-trip
        // to Normal because `y` captured it into last_visual on exit (D-027 depth-1).
        let st = run(
            "hello world",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Charwise,
                },
                Command::Move(1, Motion::InnerWord),
                Command::YankSelection,
                Command::ReselectVisual,
                Command::DeleteSelection,
            ],
        );
        assert_eq!(text(&st), " world");
        assert_eq!(st.cursor(), 0);
        assert_eq!(st.register().text(), b"hello");
        assert!(!st.register().is_linewise());
        assert_eq!(st.mode(), Mode::Normal);
    }

    #[test]
    fn gv_without_a_prior_selection_is_a_noop() {
        let st = run("abc", &[Command::ReselectVisual]);
        assert_eq!(text(&st), "abc");
        assert_eq!(st.mode(), Mode::Normal);
        assert_eq!(st.cursor(), 0);
    }

    #[test]
    fn swap_is_involutive() {
        // `oo` restores the original selection span (both ends back where they were).
        let base = run(
            "abcde",
            &[
                Command::Move(1, Motion::Right),
                Command::Move(1, Motion::Right),
                Command::Move(1, Motion::Right),
                Command::EnterVisual {
                    kind: SelectKind::Charwise,
                },
                Command::Move(1, Motion::Left),
            ],
        );
        let span_before = base.selection_span();
        let cur_before = base.cursor();

        let mut st = base;
        apply_command(&mut st, &Command::SwapSelectionEnds);
        apply_command(&mut st, &Command::SwapSelectionEnds);
        assert_eq!(st.selection_span(), span_before, "oo restores the span");
        assert_eq!(st.cursor(), cur_before, "oo restores the active end");
    }

    #[test]
    fn swap_keeps_the_selected_span() {
        // A single `o` leaves the SAME text selected — only the active end changes.
        let mut st = run(
            "abcde",
            &[
                Command::Move(1, Motion::Right),
                Command::EnterVisual {
                    kind: SelectKind::Charwise,
                },
                Command::Move(1, Motion::Right),
                Command::Move(1, Motion::Right),
            ],
        );
        let span_before = st.selection_span();
        apply_command(&mut st, &Command::SwapSelectionEnds);
        assert_eq!(st.selection_span(), span_before, "swap preserves the span");
    }

    #[test]
    fn swap_outside_a_selection_is_a_noop() {
        let st = run("abcde", &[Command::SwapSelectionEnds]);
        assert_eq!(text(&st), "abcde");
        assert_eq!(st.cursor(), 0);
        assert_eq!(st.mode(), Mode::Normal);
        assert!(st.selection_span().is_none());
    }
}

#[cfg(test)]
mod single_key_edit_tests {
    use crate::editor::*;

    fn run(initial: &str, cmds: &[Command]) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        for c in cmds {
            apply_command(&mut st, c);
        }
        st
    }

    fn text(st: &EditorState) -> String {
        String::from_utf8(st.bytes().to_vec()).expect("utf8")
    }

    #[test]
    fn replace_char_keeps_the_cursor() {
        let st = run("abc", &[Command::MoveRight, Command::ReplaceChar(1, 'X')]);
        assert_eq!(text(&st), "aXc");
        assert_eq!(st.cursor(), 1, "r leaves the cursor on the replaced char");
        assert_eq!(st.mode(), Mode::Normal);
    }

    #[test]
    fn replace_char_multibyte() {
        let st = run("abc", &[Command::ReplaceChar(1, '가')]);
        assert_eq!(text(&st), "가bc");
    }

    #[test]
    fn replace_over_newline_or_eol_is_noop() {
        // On an empty line the cursor sits at the line end (== line start): `r` has no char to replace and
        // is a clean no-op (Vim). This is the Vim-valid way to land on EOL — a bare `$` rests on the last
        // char, never past it, so it can never park the cursor on the newline itself.
        let st = run("a\n\nb", &[Command::MoveDown, Command::ReplaceChar(1, 'X')]);
        assert_eq!(text(&st), "a\n\nb", "r on an empty line's EOL does nothing");
    }

    #[test]
    fn bare_dollar_lands_on_the_last_char_not_past_it() {
        // Vim: a bare `$` rests ON the last char (byte 4 of "hello"), unlike the `d$` operator span which
        // reaches the line end. This is what makes `$d0` leave the final char (parity fixture d_to_bol).
        let st = run("hello", &[Command::Move(1, Motion::LineEnd)]);
        assert_eq!(st.cursor(), 4);
        let st = run(
            "hello world",
            &[
                Command::Move(1, Motion::LineEnd),
                Command::Delete(1, Motion::LineStart),
            ],
        );
        assert_eq!(text(&st), "d", "$d0 deletes [BOL, last char)");
        assert_eq!(st.register().text(), b"hello worl");
        assert!(!st.register().is_linewise());
    }

    #[test]
    fn replace_char_with_count() {
        // `3rz` replaces three chars and leaves the cursor on the last one.
        let st = run("abcdef", &[Command::ReplaceChar(3, 'z')]);
        assert_eq!(text(&st), "zzzdef");
        assert_eq!(st.cursor(), 2, "cursor lands on the last replaced char");
        // Fewer than `count` chars remain on the line: a clean no-op (Vim never partial-replaces).
        let st = run("ab", &[Command::ReplaceChar(3, 'z')]);
        assert_eq!(text(&st), "ab");
        assert_eq!(st.cursor(), 0);
    }

    #[test]
    fn delete_under_with_count() {
        // `3x` deletes three chars into the unnamed register (charwise); clamps at EOL.
        let st = run("abcdef", &[Command::DeleteUnder(3)]);
        assert_eq!(text(&st), "def");
        assert_eq!(st.cursor(), 0);
        assert_eq!(st.register().text(), b"abc");
        assert!(!st.register().is_linewise());
        // Fewer than `count` chars left: delete to EOL (not across the newline).
        let st = run("abc\nxy", &[Command::DeleteUnder(9)]);
        assert_eq!(text(&st), "\nxy");
    }

    #[test]
    fn toggle_case_flips_and_moves_right() {
        let st = run("aBc", &[Command::ToggleCase(1)]);
        assert_eq!(text(&st), "ABc");
        assert_eq!(st.cursor(), 1);
        // On a non-letter, `~` just moves right.
        let st = run("1a", &[Command::ToggleCase(1)]);
        assert_eq!(text(&st), "1a");
        assert_eq!(st.cursor(), 1);
    }

    #[test]
    fn toggle_case_with_count() {
        // `3~` toggles three chars, leaving the cursor past the last (clamped at EOL).
        let st = run("abcdef", &[Command::ToggleCase(3)]);
        assert_eq!(text(&st), "ABCdef");
        assert_eq!(st.cursor(), 3);
        // Clamp: fewer than `count` chars left toggles to EOL. The cursor would land past the last char,
        // but Vim never rests the Normal-mode cursor on the newline, so `commit` pulls it onto the last char.
        let st = run("aB", &[Command::ToggleCase(9)]);
        assert_eq!(text(&st), "Ab");
        assert_eq!(st.cursor(), 1);
    }

    #[test]
    fn join_lines_uses_one_space_and_drops_indent() {
        let st = run("foo\n   bar", &[Command::JoinLines]);
        assert_eq!(text(&st), "foo bar");
        assert_eq!(st.cursor(), 3, "cursor lands on the joined space");
    }

    #[test]
    fn join_on_last_line_is_noop() {
        let st = run("only", &[Command::JoinLines]);
        assert_eq!(text(&st), "only");
    }

    #[test]
    fn join_no_space_keeps_indent_and_inserts_nothing() {
        // gJ removes only the newline: the next line's leading whitespace is preserved, no space added.
        let st = run("foo\n   bar", &[Command::JoinLinesNoSpace]);
        assert_eq!(text(&st), "foo   bar");
        assert_eq!(st.cursor(), 3, "cursor rests at the join seam");
    }

    #[test]
    fn join_no_space_on_last_line_is_noop() {
        let st = run("only", &[Command::JoinLinesNoSpace]);
        assert_eq!(text(&st), "only");
    }

    #[test]
    fn increment_number_adjusts_and_lands_on_last_digit() {
        // Cursor on the '4' of "x = 4": +1 → 5.
        let st = run(
            "x = 4",
            &[Command::Move(4, Motion::Right), Command::IncrementNumber(1)],
        );
        assert_eq!(text(&st), "x = 5");
        assert_eq!(st.cursor(), 4, "cursor on the last digit of the result");
        // Cursor BEFORE the number still finds it forward on the line.
        let st = run("x = 4", &[Command::IncrementNumber(3)]);
        assert_eq!(text(&st), "x = 7");
        // A carry grows the digit count; cursor on the new last digit.
        let st = run("9", &[Command::IncrementNumber(1)]);
        assert_eq!(text(&st), "10");
        assert_eq!(st.cursor(), 1);
    }

    #[test]
    fn increment_handles_sign_and_decrement_and_noop() {
        // Decrement below zero grows a '-' sign.
        let st = run("0", &[Command::IncrementNumber(-3)]);
        assert_eq!(text(&st), "-3");
        // A leading '-' is treated as the number's sign.
        let st = run("-3", &[Command::IncrementNumber(1)]);
        assert_eq!(text(&st), "-2");
        // No number after the cursor → no-op.
        let st = run("abc", &[Command::IncrementNumber(1)]);
        assert_eq!(text(&st), "abc");
    }

    #[test]
    fn increment_hex_literal_stays_hex() {
        // 0x1f + 1 → 0x20 (prefix preserved, lowercase output).
        let st = run("0x1f", &[Command::IncrementNumber(1)]);
        assert_eq!(text(&st), "0x20");
        // Carry widens: 0xff + 1 → 0x100.
        let st = run("0xff", &[Command::IncrementNumber(1)]);
        assert_eq!(text(&st), "0x100");
        // Cursor INSIDE the hex digits (on the '1') still adjusts the whole literal.
        let st = run(
            "0x1f",
            &[Command::Move(2, Motion::Right), Command::IncrementNumber(1)],
        );
        assert_eq!(text(&st), "0x20");
        // Hex clamps at 0 (no negative hex).
        let st = run("0x0", &[Command::IncrementNumber(-5)]);
        assert_eq!(text(&st), "0x0");
        // A plain decimal next to no 0x is unaffected by the hex path.
        let st = run("value 10 end", &[Command::IncrementNumber(1)]);
        assert_eq!(text(&st), "value 11 end");
    }

    #[test]
    fn increment_binary_and_octal_stay_in_base() {
        // Binary: 0b101 (5) + 1 → 0b110, carry widens 0b111 + 1 → 0b1000.
        let st = run("0b101", &[Command::IncrementNumber(1)]);
        assert_eq!(text(&st), "0b110");
        let st = run("0b111", &[Command::IncrementNumber(1)]);
        assert_eq!(text(&st), "0b1000");
        // Octal: 0o17 (15) + 1 → 0o20; prefix + lowercase preserved.
        let st = run("0o17", &[Command::IncrementNumber(1)]);
        assert_eq!(text(&st), "0o20");
        // Cursor INSIDE the digits still adjusts the whole literal (on the '0' of 0o17's body).
        let st = run(
            "0o17",
            &[Command::Move(3, Motion::Right), Command::IncrementNumber(1)],
        );
        assert_eq!(text(&st), "0o20");
        // Both clamp at 0 (no negative based literals).
        let st = run("0b0", &[Command::IncrementNumber(-5)]);
        assert_eq!(text(&st), "0b0");
        // A capital prefix keeps its letter but lowercases the digits.
        let st = run("0B10", &[Command::IncrementNumber(1)]);
        assert_eq!(text(&st), "0B11");
    }

    #[test]
    fn goto_last_change_jumps_to_the_last_edit() {
        // Edit on line 2, move away to the top, then `` `. `` returns to the change.
        let st = run(
            "one\ntwo\nthree",
            &[
                Command::Move(1, Motion::Down),
                Command::DeleteUnder(1), // delete 't' at the start of line 2 — the last change
                Command::Move(1, Motion::Up), // move away to line 1
                Command::GotoLastChange,
            ],
        );
        // The change was at the start of line 2 (byte 4); `` `. `` lands there.
        assert_eq!(st.cursor(), 4, "`. returns to the last change position");
    }

    #[test]
    fn goto_last_change_before_any_edit_is_noop() {
        let st = run("abc", &[Command::GotoLastChange]);
        assert_eq!(st.cursor(), 0, "no last change yet → cursor unmoved");
    }

    #[test]
    fn change_list_g_semicolon_walks_older_then_g_comma_newer() {
        // Two changes: delete on line 1 (byte 0), then delete on line 3 (byte 6 after the first delete).
        // Buffer "a\nb\nc" → after x at 0: "\nb\nc"; move to line 3; x at its start.
        let st = run(
            "a\nb\nc",
            &[
                Command::DeleteUnder(1),        // change #1 at byte 0
                Command::Move(2, Motion::Down), // to line 3 ("c")
                Command::DeleteUnder(1),        // change #2 at that line's start
                Command::Move(1, Motion::Up),   // move away
            ],
        );
        let change2 = st.cursor(); // where we are is near change #2's line; capture positions via nav
                                   // g; → newest change (#2), g; again → older (#1 at byte 0), g, → back to newest (#2).
        let mut st = st;
        crate::editor::apply_command(&mut st, &Command::GotoOlderChange);
        let newest = st.cursor();
        crate::editor::apply_command(&mut st, &Command::GotoOlderChange);
        assert_eq!(
            st.cursor(),
            0,
            "second g; reaches the oldest change at byte 0"
        );
        crate::editor::apply_command(&mut st, &Command::GotoNewerChange);
        assert_eq!(st.cursor(), newest, "g, returns to the newer change");
        let _ = change2;
    }

    #[test]
    fn named_mark_set_then_jump_returns_to_it() {
        // Set mark 'a' on line 2, move to line 1, `` `a `` returns to line 2's start (byte 2).
        let st = run(
            "ab\ncd\nef",
            &[
                Command::Move(1, Motion::Down), // to line 2 ("cd"), byte 3
                Command::SetNamedMark('a'),
                Command::Move(1, Motion::Up), // away to line 1
                Command::GotoNamedMark('a'),
            ],
        );
        assert_eq!(st.cursor(), 3, "`a returns to where mark a was set");
    }

    #[test]
    fn goto_unset_named_mark_is_noop() {
        let st = run("abc", &[Command::GotoNamedMark('q')]);
        assert_eq!(st.cursor(), 0, "jumping an unset mark does not move");
    }

    #[test]
    fn apostrophe_mark_jumps_linewise_to_first_non_blank() {
        // Line 2 is "  xy" (2-space indent). Set mark a on the 'y' (byte 7), leave to line 1, then
        // `'a` lands on the FIRST NON-BLANK of line 2 (byte 5, the 'x'), not the exact mark column.
        let st = run(
            "ab\n  xy",
            &[
                Command::Move(1, Motion::Down),  // line 2
                Command::Move(3, Motion::Right), // onto 'y' at byte 7
                Command::SetNamedMark('a'),
                Command::Move(1, Motion::Up), // away
                Command::GotoNamedMarkLine('a'),
            ],
        );
        assert_eq!(
            st.cursor(),
            5,
            "'a lands on the line's first non-blank, not the mark column"
        );
    }

    #[test]
    fn apostrophe_last_change_line_is_linewise() {
        // Edit on an indented line 2, move away, `'.` returns to its first non-blank.
        let st = run(
            "ab\n   cd",
            &[
                Command::Move(1, Motion::Down),
                Command::Move(4, Motion::Right), // onto 'd' (byte 7)
                Command::DeleteUnder(1),         // change on line 2
                Command::Move(1, Motion::Up),
                Command::GotoLastChangeLine,
            ],
        );
        assert_eq!(
            st.cursor(),
            6,
            "'. lands on line 2's first non-blank (byte 6, the 'c')"
        );
    }

    #[test]
    fn gi_resumes_insert_at_the_last_insert_position() {
        // Insert "XY" at the start of line 2, leave Insert, move to line 1, `gi` returns to Insert there.
        let st = run(
            "ab\ncd",
            &[
                Command::Move(1, Motion::Down), // line 2 start (byte 3)
                Command::EnterInsert,
                Command::InsertChar('X'),
                Command::InsertChar('Y'), // caret now at byte 5
                Command::EnterNormal,     // leaving Insert records `^ at the caret
                Command::Move(1, Motion::Up),
                Command::InsertAtLastInsert,
            ],
        );
        assert_eq!(st.mode(), Mode::Insert, "gi enters Insert");
        assert_eq!(st.cursor(), 5, "gi resumes at the last-insert caret");
        assert_eq!(text(&st), "ab\nXYcd", "no text change from gi itself");
    }

    #[test]
    fn gi_before_any_insert_goes_to_start() {
        let st = run("abc", &[Command::InsertAtLastInsert]);
        assert_eq!(st.mode(), Mode::Insert);
        assert_eq!(st.cursor(), 0, "no prior insert → start of buffer");
    }

    #[test]
    fn named_mark_snaps_after_a_shrinking_edit() {
        // Mark near the end, then delete most of the buffer; the mark must stay in range (no panic on jump).
        let st = run(
            "hello world",
            &[
                Command::Move(9, Motion::Right), // near the end
                Command::SetNamedMark('a'),
                Command::Move(9, Motion::Left), // back to start
                Command::DeleteForward(8),      // shrink the buffer under the mark
                Command::GotoNamedMark('a'),
            ],
        );
        assert!(
            st.cursor() <= text(&st).len(),
            "mark jump stays in range after shrink"
        );
    }

    #[test]
    fn jumplist_records_jumps_and_ctrl_o_ctrl_i_walk_it() {
        // Lines: "one\ntwo\nthree\nfour" — jump gg (to line 1) then G (to last line), building the jumplist,
        // then CTRL-O walks back and CTRL-I forward.
        let src = "one\ntwo\nthree\nfour";
        // Start on line 2, jump to line 1 (gg), then to last line (G).
        let mut st = crate::editor::EditorState::new(src.as_bytes().to_vec());
        crate::editor::apply_command(&mut st, &Command::Move(2, Motion::Down)); // → line 3 area (not a jump)
        let before_gg = st.cursor();
        crate::editor::apply_command(&mut st, &Command::Move(1, Motion::GotoLine)); // gg → line 1 (a jump)
        assert_eq!(st.cursor(), 0, "gg to line 1");
        crate::editor::apply_command(&mut st, &Command::Move(0, Motion::LastLine)); // G → last line (a jump)
        let at_last = st.cursor();
        // CTRL-O: first back saves the current (last line), steps to the newest recorded jump (line-1 pos 0).
        crate::editor::apply_command(&mut st, &Command::GotoOlderJump);
        assert_eq!(st.cursor(), 0, "CTRL-O returns to the gg position");
        // CTRL-O again: to the position gg jumped FROM (before_gg).
        crate::editor::apply_command(&mut st, &Command::GotoOlderJump);
        assert_eq!(st.cursor(), before_gg, "second CTRL-O to where gg started");
        // CTRL-I forward returns toward the newer jumps.
        crate::editor::apply_command(&mut st, &Command::GotoNewerJump);
        assert_eq!(st.cursor(), 0, "CTRL-I forward to the gg position");
        crate::editor::apply_command(&mut st, &Command::GotoNewerJump);
        assert_eq!(
            st.cursor(),
            at_last,
            "CTRL-I forward to the saved last-line position"
        );
    }

    #[test]
    fn jumplist_nav_is_noop_without_jumps() {
        let st = run("abc", &[Command::GotoOlderJump, Command::GotoNewerJump]);
        assert_eq!(st.cursor(), 0, "nothing recorded → no movement");
    }

    #[test]
    fn plain_motions_do_not_record_jumps() {
        // h/j/k/l/w are NOT jumps, so CTRL-O after them does nothing.
        let st = run(
            "hello world",
            &[Command::Move(3, Motion::Right), Command::GotoOlderJump],
        );
        assert_eq!(
            st.cursor(),
            3,
            "a plain motion is not a jump; CTRL-O is a no-op"
        );
    }

    #[test]
    fn change_list_nav_is_noop_at_the_ends_and_before_edits() {
        // No edits → g;/g, do nothing.
        let st = run("abc", &[Command::GotoOlderChange, Command::GotoNewerChange]);
        assert_eq!(st.cursor(), 0, "no changes → nav is a no-op");
        // One edit, then g, (newer) with nothing newer is a no-op; g; reaches it.
        let mut st = run("abcd", &[Command::DeleteUnder(1)]);
        let at_edit = st.cursor();
        crate::editor::apply_command(&mut st, &Command::GotoNewerChange);
        assert_eq!(st.cursor(), at_edit, "g, at the newest end does not move");
        crate::editor::apply_command(&mut st, &Command::GotoOlderChange);
        assert_eq!(st.cursor(), at_edit, "g; lands on the single change");
    }
}

#[cfg(test)]
mod shift_tests {
    use crate::editor::*;

    fn run(initial: &str, cmds: &[Command]) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        for c in cmds {
            apply_command(&mut st, c);
        }
        st
    }

    fn run_indent(initial: &str, tw: usize, style: IndentStyle, cmds: &[Command]) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        st.set_indent(tw, style);
        for c in cmds {
            apply_command(&mut st, c);
        }
        st
    }

    fn text(st: &EditorState) -> String {
        String::from_utf8(st.bytes().to_vec()).expect("utf8")
    }

    #[test]
    fn shift_right_adds_tab_width_spaces_and_homes_to_first_non_blank() {
        // Default config = 4 spaces. Matches the parity fixture shift_right_line (oracle `>>`).
        let st = run("hello", &[Command::ShiftRight(1)]);
        assert_eq!(text(&st), "    hello");
        assert_eq!(st.cursor(), 4, "cursor lands on the first non-blank ('h')");
        assert_eq!(
            st.register().text(),
            b"",
            "shift does not touch the register"
        );
    }

    #[test]
    fn shift_right_stacks_onto_existing_indent() {
        let st = run("  hi", &[Command::ShiftRight(1)]);
        assert_eq!(text(&st), "      hi"); // 2 + 4 spaces
        assert_eq!(st.cursor(), 6);
    }

    #[test]
    fn shift_right_uses_a_tab_when_indent_style_is_tab() {
        let st = run_indent("hello", 4, IndentStyle::Tab, &[Command::ShiftRight(1)]);
        assert_eq!(text(&st), "\thello");
        assert_eq!(st.cursor(), 1, "cursor after the one-byte tab, on 'h'");
    }

    #[test]
    fn shift_right_leaves_a_truly_empty_line_untouched() {
        let st = run("", &[Command::ShiftRight(1)]);
        assert_eq!(text(&st), "", "Vim never indents an empty line");
        assert_eq!(st.cursor(), 0);
    }

    #[test]
    fn shift_right_count_shifts_multiple_lines_cursor_stays_on_first() {
        let st = run("a\nb\nc", &[Command::ShiftRight(2)]);
        assert_eq!(
            text(&st),
            "    a\n    b\nc",
            "2>> shifts the first two lines"
        );
        assert_eq!(
            st.cursor(),
            4,
            "cursor stays on the first line's first non-blank"
        );
    }

    #[test]
    fn shift_left_removes_one_level_of_spaces() {
        let st = run("    hello", &[Command::ShiftLeft(1)]);
        assert_eq!(text(&st), "hello");
        assert_eq!(st.cursor(), 0);
    }

    #[test]
    fn shift_left_removes_at_most_one_level() {
        // 6 leading spaces, tab_width 4 -> removes 4, leaves 2.
        let st = run("      hi", &[Command::ShiftLeft(1)]);
        assert_eq!(text(&st), "  hi");
        assert_eq!(st.cursor(), 2);
    }

    #[test]
    fn shift_left_partial_indent_never_crosses_column_zero() {
        let st = run("  hi", &[Command::ShiftLeft(1)]);
        assert_eq!(
            text(&st),
            "hi",
            "fewer than tab_width spaces: remove them all, no more"
        );
        assert_eq!(st.cursor(), 0);
    }

    #[test]
    fn shift_left_on_unindented_line_is_a_noop() {
        let st = run("hi", &[Command::ShiftLeft(1)]);
        assert_eq!(text(&st), "hi");
        assert_eq!(st.cursor(), 0);
    }

    #[test]
    fn shift_left_removes_a_leading_tab() {
        let st = run("\thello", &[Command::ShiftLeft(1)]);
        assert_eq!(text(&st), "hello");
        assert_eq!(st.cursor(), 0);
    }

    #[test]
    fn shift_right_then_left_round_trips() {
        let st = run("hello", &[Command::ShiftRight(1), Command::ShiftLeft(1)]);
        assert_eq!(text(&st), "hello", ">> then << restores the original");
        assert_eq!(st.cursor(), 0);
    }

    #[test]
    fn shift_is_undoable_as_one_edit() {
        let st = run("a\nb", &[Command::ShiftRight(2), Command::Undo]);
        assert_eq!(text(&st), "a\nb", "a single undo reverses the whole shift");
    }
}

#[cfg(test)]
mod insert_entry_tests {
    use crate::editor::*;

    fn run(initial: &str, cmds: &[Command]) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        for c in cmds {
            apply_command(&mut st, c);
        }
        st
    }

    fn text(st: &EditorState) -> String {
        String::from_utf8(st.bytes().to_vec()).expect("utf8")
    }

    #[test]
    fn open_below_inserts_a_line_and_enters_insert() {
        let st = run("ab\ncd", &[Command::OpenBelow, Command::InsertChar('X')]);
        assert_eq!(text(&st), "ab\nX\ncd");
        assert_eq!(st.mode(), Mode::Insert);
    }

    #[test]
    fn undo_via_insert_one_shot_returns_to_insert() {
        // `i_CTRL-O u`: the engine borrows Undo through the Insert one-shot (KL-OBL-5). Undo restores
        // the document but must NOT drop out of Insert — it is only reachable from Insert via that
        // one-shot (Vim's `u` in Insert is literal text), so undo/redo preserve Insert rather than
        // hardcoding Normal. Regression for the session-fuzzer finding on `i h CTRL-O u`.
        let st = run(
            "",
            &[
                Command::EnterInsert,
                Command::InsertChar('h'),
                Command::Undo, // stands in for the one-shot-borrowed `u`
            ],
        );
        assert_eq!(text(&st), "", "undo removes the inserted char");
        assert_eq!(
            st.mode(),
            Mode::Insert,
            "i_CTRL-O u must resume Insert, not fall to Normal"
        );
    }

    #[test]
    fn undo_from_normal_stays_in_normal() {
        // The other side of the fix: a plain Normal-mode undo must still land in Normal.
        let st = run("x", &[Command::DeleteUnder(1), Command::Undo]);
        assert_eq!(text(&st), "x", "undo restores the deleted char");
        assert_eq!(
            st.mode(),
            Mode::Normal,
            "a Normal-mode undo stays in Normal"
        );
    }

    #[test]
    fn open_below_on_last_line() {
        let st = run("ab", &[Command::OpenBelow, Command::InsertChar('X')]);
        assert_eq!(text(&st), "ab\nX");
    }

    #[test]
    fn open_above_inserts_before_the_line() {
        // On line 2 ('cd'); O opens a line above it.
        let st = run(
            "ab\ncd",
            &[
                Command::MoveDown,
                Command::OpenAbove,
                Command::InsertChar('X'),
            ],
        );
        assert_eq!(text(&st), "ab\nX\ncd");
    }

    #[test]
    fn apply_edits_replaces_disjoint_ranges_as_one_undo_group() {
        let mut st = EditorState::new(b"hello world".to_vec());
        st.apply_edits(
            &[
                (0, 5, "Hi".to_string()),     // "hello" → "Hi"
                (6, 11, "there".to_string()), // "world" → "there"
            ],
            crate::TransactionOrigin::Lsp,
        );
        assert_eq!(text(&st), "Hi there");
        // The whole set is one undo group (LSP origin).
        apply_command(&mut st, &Command::Undo);
        assert_eq!(text(&st), "hello world");
    }

    #[test]
    fn apply_edits_batches_are_separate_undo_units_regardless_of_origin() {
        // Each apply_edits call is its own undo group (GroupHint::BreakBefore), so a later batch never
        // coalesces into an earlier one — independent of the caller-supplied origin (#305). This is the
        // contract the LSP format/rename/code-action flows rely on: applying LSP edits leaves the user's
        // prior edits as a distinct, still-undoable unit.
        let mut st = EditorState::new(b"abc".to_vec());
        st.apply_edits(
            &[(3, 3, "X".to_string())],
            crate::TransactionOrigin::UserInput,
        ); // "abcX"
        st.apply_edits(&[(0, 0, "Y".to_string())], crate::TransactionOrigin::Lsp); // "YabcX"
        assert_eq!(text(&st), "YabcX");
        apply_command(&mut st, &Command::Undo); // undoes ONLY the LSP batch
        assert_eq!(text(&st), "abcX");
        apply_command(&mut st, &Command::Undo); // then the earlier batch
        assert_eq!(text(&st), "abc");
    }

    #[test]
    fn apply_edits_skips_overlapping_and_out_of_range() {
        let mut st = EditorState::new(b"abcdef".to_vec());
        let lsp = crate::TransactionOrigin::Lsp;
        st.apply_edits(&[(0, 3, "X".to_string()), (2, 5, "Y".to_string())], lsp); // overlap → refused
        assert_eq!(text(&st), "abcdef");
        st.apply_edits(&[(0, 99, "Z".to_string())], lsp); // out of range → skipped
        assert_eq!(text(&st), "abcdef");
    }

    #[test]
    fn open_line_indent_seeds_leading_whitespace() {
        // `o` with level 2 opens a new line below of 2×tab_width (=8) spaces, cursor after them.
        let st = run(
            "ab\ncd",
            &[
                Command::OpenLineIndent {
                    kind: OpenKind::Below,
                    level: 2,
                },
                Command::InsertChar('X'),
            ],
        );
        assert_eq!(text(&st), "ab\n        X\ncd");
        assert_eq!(st.mode(), Mode::Insert);
    }

    #[test]
    fn open_line_indent_above_seeds_before_the_line() {
        // `O` (Above) on line 2 opens an indented blank line before it.
        let st = run(
            "ab\ncd",
            &[
                Command::MoveDown,
                Command::OpenLineIndent {
                    kind: OpenKind::Above,
                    level: 1,
                },
                Command::InsertChar('X'),
            ],
        );
        assert_eq!(text(&st), "ab\n    X\ncd");
    }

    #[test]
    fn open_line_indent_split_carries_the_tail_down() {
        // `<CR>` (Split) at the cursor: the tail moves onto a new indented line.
        let st = run(
            "abcd",
            &[
                Command::OpenLineIndent {
                    kind: OpenKind::Split,
                    level: 1,
                },
                Command::InsertChar('X'),
            ],
        );
        // Cursor starts at byte 0; split there → "\n    " then X → tail "abcd" follows.
        assert_eq!(text(&st), "\n    Xabcd");
    }

    #[test]
    fn open_line_indent_level_zero_is_a_plain_open() {
        // The non-tree fallback: level 0 seeds no whitespace, identical to `OpenBelow`.
        let st = run(
            "ab",
            &[
                Command::OpenLineIndent {
                    kind: OpenKind::Below,
                    level: 0,
                },
                Command::InsertChar('X'),
            ],
        );
        assert_eq!(text(&st), "ab\nX");
    }

    // Like `run`, but seeds the cursor at the end of `initial` (where a closer would be typed).
    fn run_at_end(initial: &str, cmds: &[Command]) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        st.set_cursor(initial.len());
        for c in cmds {
            apply_command(&mut st, c);
        }
        st
    }

    #[test]
    fn insert_closer_realigns_to_matching_opener() {
        // Cursor sits on an over-indented blank line inside the block; typing `}` realigns it under `fn`.
        let st = run_at_end("fn f() {\n        ", &[Command::InsertCloser { ch: '}' }]);
        assert_eq!(text(&st), "fn f() {\n}");
        assert_eq!(st.mode(), Mode::Insert);
    }

    #[test]
    fn insert_closer_aligns_to_the_openers_own_indent() {
        // `)` has no matching `(` → plain insert (leading whitespace untouched).
        let st = run_at_end(
            "    if x {\n            ",
            &[Command::InsertCloser { ch: ')' }],
        );
        assert_eq!(text(&st), "    if x {\n            )");
        // `}` matches the `{` on the 4-indented line → realign to that line's 4 spaces.
        let st = run_at_end(
            "    if x {\n            ",
            &[Command::InsertCloser { ch: '}' }],
        );
        assert_eq!(text(&st), "    if x {\n    }");
    }

    #[test]
    fn insert_closer_with_content_before_cursor_is_a_plain_insert() {
        // The `}` is not the sole leading content (there's `x` before it), so no realignment.
        let st = run_at_end("a {}\n    x", &[Command::InsertCloser { ch: '}' }]);
        assert_eq!(text(&st), "a {}\n    x}");
    }

    #[test]
    fn insert_closer_without_a_matching_opener_is_a_plain_insert() {
        let st = run_at_end("    ", &[Command::InsertCloser { ch: '}' }]);
        assert_eq!(text(&st), "    }");
    }

    #[test]
    fn append_goes_to_line_end() {
        // On 'a' of "ab"; A appends at the end.
        let st = run(
            "ab\ncd",
            &[Command::AppendLineEnd, Command::InsertChar('X')],
        );
        assert_eq!(text(&st), "abX\ncd");
        assert_eq!(st.mode(), Mode::Insert);
    }

    #[test]
    fn insert_line_start_goes_to_first_non_blank() {
        // Cursor at end of a leading-blank line; I jumps to the first non-blank.
        let st = run(
            "  ab",
            &[
                Command::Move(1, Motion::LineEnd),
                Command::InsertLineStart,
                Command::InsertChar('X'),
            ],
        );
        assert_eq!(text(&st), "  Xab", "I inserts before the first non-blank");
    }
}

#[cfg(test)]
mod word_class_tests {
    use crate::editor::*;

    fn run(initial: &str, cmds: &[Command]) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        for c in cmds {
            apply_command(&mut st, c);
        }
        st
    }

    fn text(st: &EditorState) -> String {
        String::from_utf8(st.bytes().to_vec()).expect("utf8")
    }

    #[test]
    fn small_word_stops_at_punctuation() {
        // "foo.bar baz": w → '.', w → 'bar', w → 'baz'.
        let st = run("foo.bar baz", &[Command::Move(1, Motion::WordFwd)]);
        assert_eq!(st.cursor(), 3, "w stops on the '.'");
        let st = run(
            "foo.bar baz",
            &[
                Command::Move(1, Motion::WordFwd),
                Command::Move(1, Motion::WordFwd),
            ],
        );
        assert_eq!(st.cursor(), 4, "second w → start of 'bar'");
    }

    #[test]
    fn big_word_spans_punctuation() {
        // "foo.bar baz": W → 'baz' (foo.bar is one WORD).
        let st = run("foo.bar baz", &[Command::Move(1, Motion::BigWordFwd)]);
        assert_eq!(st.cursor(), 8);
    }

    #[test]
    fn small_word_back_treats_punct_as_a_word() {
        // cursor on 'b' of bar (4); b → the '.' word at 3.
        let st = run(
            "foo.bar",
            &[
                Command::Move(4, Motion::Right),
                Command::Move(1, Motion::WordBack),
            ],
        );
        assert_eq!(st.cursor(), 3);
    }

    #[test]
    fn dw_small_deletes_to_the_punctuation() {
        let st = run("foo.bar", &[Command::Delete(1, Motion::WordFwd)]);
        assert_eq!(text(&st), ".bar", "dw deletes 'foo' up to the '.'");
    }

    #[test]
    fn dbigw_deletes_the_whole_word() {
        let st = run("foo.bar baz", &[Command::Delete(1, Motion::BigWordFwd)]);
        assert_eq!(text(&st), "baz", "dW deletes 'foo.bar ' entirely");
    }

    #[test]
    fn multibyte_is_one_word() {
        // "가나 다": w skips the Hangul word to the next.
        let st = run("가나 다", &[Command::Move(1, Motion::WordFwd)]);
        assert_eq!(
            st.cursor(),
            7,
            "w lands on '다' after the multibyte word + space"
        );
    }

    #[test]
    fn whitespace_only_text_is_unchanged_from_word_style() {
        // The pre-existing WORD behavior is preserved for plain words.
        let st = run("foo bar baz", &[Command::Move(2, Motion::WordFwd)]);
        assert_eq!(st.cursor(), 8);
    }
}

#[cfg(test)]
mod bracket_match_tests {
    use crate::editor::*;

    fn run(initial: &str, cmds: &[Command]) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        for c in cmds {
            apply_command(&mut st, c);
        }
        st
    }

    fn text(st: &EditorState) -> String {
        String::from_utf8(st.bytes().to_vec()).expect("utf8")
    }

    fn pct() -> Command {
        Command::Move(1, Motion::MatchBracket)
    }

    #[test]
    fn jumps_between_a_pair_both_ways() {
        // "a(bc)d": '(' at 1, ')' at 4.
        let st = run("a(bc)d", &[Command::MoveRight, pct()]);
        assert_eq!(st.cursor(), 4, "from '(' to ')'");
        let st = run(
            "a(bc)d",
            &[Command::Move(4, Motion::Right), pct()], // cursor onto ')'
        );
        assert_eq!(st.cursor(), 1, "from ')' back to '('");
    }

    #[test]
    fn respects_nesting() {
        // "((x))": outer '(' at 0 ↔ ')' at 4; inner '(' at 1 ↔ ')' at 3.
        let st = run("((x))", &[pct()]);
        assert_eq!(st.cursor(), 4);
        let st = run("((x))", &[Command::MoveRight, pct()]);
        assert_eq!(st.cursor(), 3);
    }

    #[test]
    fn finds_first_bracket_forward_when_not_on_one() {
        // cursor at 0 ('a'), first bracket forward is '(' at 2, its match ')' at 5.
        let st = run("ab(cd)", &[pct()]);
        assert_eq!(st.cursor(), 5);
    }

    #[test]
    fn matches_across_lines() {
        let st = run("(\n)", &[pct()]);
        assert_eq!(st.cursor(), 2, "% matches across a newline");
    }

    #[test]
    fn matches_by_type_ignoring_other_brackets() {
        // "([)]": '(' at 0 matches ')' at 2, ignoring the '[' — same as Vim.
        let st = run("([)]", &[pct()]);
        assert_eq!(st.cursor(), 2);
    }

    #[test]
    fn d_percent_is_inclusive() {
        // On '(' (index 1); d% deletes "(bc)" inclusive → "ad".
        let st = run(
            "a(bc)d",
            &[Command::MoveRight, Command::Delete(1, Motion::MatchBracket)],
        );
        assert_eq!(text(&st), "ad");
    }

    #[test]
    fn unmatched_bracket_is_a_noop() {
        let st = run("a(b", &[Command::MoveRight, pct()]);
        assert_eq!(st.cursor(), 1, "no closer → no move");
    }
}

#[cfg(test)]
mod line_jump_tests {
    use crate::editor::*;

    fn run(initial: &str, cmds: &[Command]) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        for c in cmds {
            apply_command(&mut st, c);
        }
        st
    }

    fn text(st: &EditorState) -> String {
        String::from_utf8(st.bytes().to_vec()).expect("utf8")
    }

    #[test]
    fn gg_goes_to_first_line_first_non_blank() {
        // Start on line 3; gg → first non-blank of line 1 (past the two spaces).
        let st = run(
            "  abc\ndef\nghi",
            &[
                Command::MoveDown,
                Command::MoveDown,
                Command::Move(1, Motion::GotoLine),
            ],
        );
        assert_eq!(st.cursor(), 2, "gg lands on the first non-blank of line 1");
    }

    #[test]
    fn cap_g_goes_to_last_line() {
        let st = run("abc\ndef\nxyz", &[Command::Move(1, Motion::LastLine)]);
        assert_eq!(
            st.cursor(),
            8,
            "G lands on the start of the last line 'xyz'"
        );
    }

    #[test]
    fn count_g_goes_to_that_line() {
        // {2}G → line 2 ('def' starts at byte 4).
        let st = run("abc\ndef\nghi", &[Command::Move(2, Motion::GotoLine)]);
        assert_eq!(st.cursor(), 4);
    }

    #[test]
    fn dg_deletes_linewise_to_last_line() {
        // On line 2; dG deletes lines 2..end.
        let st = run(
            "one\ntwo\nthree\n",
            &[Command::MoveDown, Command::Delete(1, Motion::LastLine)],
        );
        assert_eq!(text(&st), "one\n");
    }

    #[test]
    fn dgg_deletes_linewise_to_first_line() {
        // On line 2; dgg deletes lines 1..=2.
        let st = run(
            "one\ntwo\nthree\n",
            &[Command::MoveDown, Command::Delete(1, Motion::GotoLine)],
        );
        assert_eq!(text(&st), "three\n");
    }

    #[test]
    fn count_beyond_end_clamps_to_last_line() {
        let st = run("a\nb\n", &[Command::Move(99, Motion::GotoLine)]);
        // line 99 doesn't exist → clamp to the last line (the empty line after the final newline).
        assert_eq!(st.cursor(), 4);
    }
}

#[cfg(test)]
mod find_char_tests {
    use crate::editor::*;

    fn find(ch: char, forward: bool, till: bool) -> Motion {
        Motion::FindChar { ch, forward, till }
    }

    fn run(initial: &str, cmds: &[Command]) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        for c in cmds {
            apply_command(&mut st, c);
        }
        st
    }

    fn text(st: &EditorState) -> String {
        String::from_utf8(st.bytes().to_vec()).expect("utf8")
    }

    #[test]
    fn f_lands_on_the_char_t_stops_before() {
        // "abcxef", cursor 0.
        let st = run("abcxef", &[Command::Move(1, find('x', true, false))]);
        assert_eq!(st.cursor(), 3, "fx lands on x");
        let st = run("abcxef", &[Command::Move(1, find('x', true, true))]);
        assert_eq!(st.cursor(), 2, "tx stops one before x");
    }

    #[test]
    fn count_finds_the_nth() {
        let st = run("axbxcx", &[Command::Move(2, find('x', true, false))]);
        assert_eq!(st.cursor(), 3, "2fx lands on the second x");
    }

    #[test]
    fn backward_find() {
        // "abxde", cursor at end (4). F x → lands on x (index 2).
        let st = run(
            "abxde",
            &[
                Command::MoveLineEnd,
                Command::Move(1, find('x', false, false)),
            ],
        );
        assert_eq!(st.cursor(), 2, "Fx searches backward onto x");
    }

    #[test]
    fn dfx_deletes_through_the_char_dtx_up_to_it() {
        let st = run("abcxef", &[Command::Delete(1, find('x', true, false))]);
        assert_eq!(text(&st), "ef", "dfx deletes through x");
        let st = run("abcxef", &[Command::Delete(1, find('x', true, true))]);
        assert_eq!(text(&st), "xef", "dtx deletes up to but not including x");
    }

    #[test]
    fn stays_within_the_line() {
        // The x on the next line must not be found from line 1.
        let st = run("abc\nxyz", &[Command::Move(1, find('x', true, false))]);
        assert_eq!(st.cursor(), 0, "f does not cross the newline");
    }

    #[test]
    fn multibyte_target() {
        // "a가b" — find the multibyte '가' (bytes 1..4).
        let st = run("a가b", &[Command::Move(1, find('가', true, false))]);
        assert_eq!(st.cursor(), 1, "lands on the multibyte char boundary");
    }

    #[test]
    fn missing_target_is_a_noop() {
        let st = run("abc", &[Command::Move(1, find('z', true, false))]);
        assert_eq!(st.cursor(), 0);
    }
}

#[cfg(test)]
mod visual_tests {
    use crate::editor::*;

    fn run(initial: &str, cmds: &[Command]) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        for c in cmds {
            apply_command(&mut st, c);
        }
        st
    }

    fn text(st: &EditorState) -> String {
        String::from_utf8(st.bytes().to_vec()).expect("utf8")
    }

    #[test]
    fn entering_visual_sets_a_collapsed_selection() {
        let st = run(
            "hello",
            &[Command::EnterVisual {
                kind: SelectKind::Charwise,
            }],
        );
        assert_eq!(
            st.mode(),
            Mode::Visual {
                kind: SelectKind::Charwise
            }
        );
        // Anchor == cursor: the selection covers exactly the character under the caret (inclusive).
        assert_eq!(st.selection_span(), Some((0, 1)));
    }

    #[test]
    fn motion_extends_the_selection_and_stays_visual() {
        let st = run(
            "hello",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Charwise,
                },
                Command::MoveRight,
                Command::MoveRight,
            ],
        );
        assert_eq!(
            st.mode(),
            Mode::Visual {
                kind: SelectKind::Charwise
            }
        );
        assert_eq!(st.selection_span(), Some((0, 3)), "v + l + l selects 'hel'");
    }

    #[test]
    fn charwise_delete_over_selection() {
        let st = run(
            "hello",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Charwise,
                },
                Command::MoveRight,
                Command::MoveRight,
                Command::DeleteSelection,
            ],
        );
        assert_eq!(text(&st), "lo");
        assert_eq!(st.mode(), Mode::Normal);
        assert_eq!(st.register().text(), b"hel");
        assert_eq!(st.selection_span(), None, "selection cleared on exit");
    }

    #[test]
    fn charwise_yank_then_paste() {
        let st = run(
            "hello",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Charwise,
                },
                Command::MoveRight, // select "he"
                Command::YankSelection,
                Command::Paste {
                    after: true,
                    count: 1,
                    move_after: false,
                },
            ],
        );
        // Yank leaves the buffer, cursor at selection start (0); `p` inserts "he" after the cursor char.
        assert_eq!(st.register().text(), b"he");
        assert_eq!(text(&st), "hheello");
    }

    #[test]
    fn linewise_delete_over_two_lines() {
        let st = run(
            "a\nb\nc\n",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Linewise,
                },
                Command::MoveDown, // extend selection to the second line
                Command::DeleteSelection,
            ],
        );
        assert_eq!(text(&st), "c\n");
        assert!(st.register().is_linewise());
        assert_eq!(st.register().text(), b"a\nb\n");
    }

    #[test]
    fn change_selection_enters_insert() {
        let st = run(
            "hello",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Charwise,
                },
                Command::MoveRight,
                Command::ChangeSelection,
            ],
        );
        assert_eq!(text(&st), "llo");
        assert_eq!(st.mode(), Mode::Insert);
    }

    #[test]
    fn esc_leaves_visual_without_editing() {
        let st = run(
            "hello",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Charwise,
                },
                Command::MoveRight,
                Command::EnterNormal,
            ],
        );
        assert_eq!(text(&st), "hello");
        assert_eq!(st.mode(), Mode::Normal);
        assert_eq!(st.selection_span(), None);
    }
}

#[cfg(test)]
mod blockwise_tests {
    use crate::editor::*;

    fn run(initial: &str, cmds: &[Command]) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        for c in cmds {
            apply_command(&mut st, c);
        }
        st
    }

    fn text(st: &EditorState) -> String {
        String::from_utf8(st.bytes().to_vec()).expect("utf8")
    }

    // Enter blockwise Visual, extend right by `r` columns and down by `d` lines.
    fn block(cols_right: usize, lines_down: usize) -> Vec<Command> {
        let mut c = vec![Command::EnterVisual {
            kind: SelectKind::Blockwise,
        }];
        c.extend(std::iter::repeat_n(Command::MoveRight, cols_right));
        c.extend(std::iter::repeat_n(Command::MoveDown, lines_down));
        c
    }

    #[test]
    fn block_spans_cover_one_column_range_per_row() {
        // A 2-wide × 2-tall block from the top-left corner spans cols [0,1] on rows 0 and 1.
        let st = run("abc\ndef\nghi", &block(1, 1));
        assert_eq!(
            st.mode(),
            Mode::Visual {
                kind: SelectKind::Blockwise
            }
        );
        assert_eq!(st.block_spans(), Some(vec![(0, 2), (4, 6)]));
        assert_eq!(
            st.selection_span(),
            None,
            "a rectangle is not one contiguous span"
        );
    }

    #[test]
    fn block_delete_removes_the_column_on_each_row() {
        let mut cmds = block(1, 1);
        cmds.push(Command::DeleteSelection);
        let st = run("abc\ndef\nghi", &cmds);
        assert_eq!(text(&st), "c\nf\nghi");
        assert_eq!(st.mode(), Mode::Normal);
        assert!(st.register().is_blockwise());
        assert_eq!(st.register().text(), b"ab\nde");
        assert_eq!(st.cursor(), 0, "cursor at the block's top-left");
    }

    #[test]
    fn block_yank_then_paste_before_reproduces_the_rectangle() {
        let mut cmds = block(1, 1);
        cmds.push(Command::YankSelection);
        cmds.push(Command::Paste {
            after: false,
            count: 1,
            move_after: false,
        });
        let st = run("abc\ndef\nghi", &cmds);
        // Yank leaves the buffer; `P` drops "ab"/"de" back at column 0 on rows 0 and 1.
        assert!(st.register().is_blockwise());
        assert_eq!(text(&st), "ababc\ndedef\nghi");
        assert_eq!(st.cursor(), 0);
    }

    #[test]
    fn block_paste_after_inserts_one_column_right() {
        // Yank the single column "a"/"d", then `p` pastes it to the right of the cursor on each row.
        let st = run(
            "abc\ndef",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Blockwise,
                },
                Command::MoveDown,
                Command::YankSelection,
                Command::Paste {
                    after: true,
                    count: 1,
                    move_after: false,
                },
            ],
        );
        assert_eq!(st.register().text(), b"a\nd");
        assert_eq!(text(&st), "aabc\nddef");
    }

    #[test]
    fn block_paste_extends_past_the_last_line() {
        // A 2-row block pasted onto a 1-line buffer creates the missing second line.
        let st = run(
            "ab\ncd",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Blockwise,
                },
                Command::MoveDown, // column 0, rows 0..1 -> "a"/"c"
                Command::YankSelection,
                Command::EnterInsert, // bail out of any selection state
                Command::EnterNormal,
                Command::Paste {
                    after: false,
                    count: 1,
                    move_after: false,
                },
            ],
        );
        // Cursor is back on row 0 col 0 after the yank; `P` pastes "a"/"c" at column 0 on rows 0 and 1.
        assert_eq!(text(&st), "aab\nccd");
    }

    #[test]
    fn block_rows_clamp_a_short_line_to_an_empty_range() {
        // Geometry directly (motions through a short line collapse the tracked column — the deferred
        // curswant gap — so exercise `block_rows` on explicit corners). Cols [1,3] over three rows; the
        // middle line "x" is shorter than col 1, so it contributes an empty range at its end.
        let b = b"abcd\nx\nefgh";
        let (rows, lo, hi) = block_rows(b, 3, 8); // (row0,col3) .. (row2,col1)
        assert_eq!((lo, hi), (1, 3));
        assert_eq!(rows, vec![(1, 4), (6, 6), (8, 11)]);
        assert_eq!(&b[1..4], b"bcd");
        assert_eq!(&b[8..11], b"fgh");
    }

    #[test]
    fn block_insert_i_replicates_typed_text_down_all_rows() {
        let st = run(
            "abc\ndef\nghi",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Blockwise,
                },
                Command::MoveDown,
                Command::MoveDown, // column 0, rows 0..2
                Command::BlockInsert(BlockInsertKind::Insert),
                Command::InsertChar('X'),
                Command::EnterNormal,
            ],
        );
        assert_eq!(text(&st), "Xabc\nXdef\nXghi");
        assert_eq!(st.mode(), Mode::Normal);
        assert_eq!(st.cursor(), 0, "cursor returns to the block top-left");
    }

    #[test]
    fn block_append_a_replicates_after_the_right_edge() {
        let st = run(
            "abc\ndef",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Blockwise,
                },
                Command::MoveDown, // column 0, rows 0..1
                Command::BlockInsert(BlockInsertKind::Append),
                Command::InsertChar('X'),
                Command::EnterNormal,
            ],
        );
        // Append column is one past the block's right edge (col 0) → col 1 on both rows.
        assert_eq!(text(&st), "aXbc\ndXef");
    }

    #[test]
    fn block_change_c_deletes_then_replicates() {
        let st = run(
            "abc\ndef",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Blockwise,
                },
                Command::MoveDown, // column 0, rows 0..1
                Command::BlockInsert(BlockInsertKind::Change),
                Command::InsertChar('X'),
                Command::EnterNormal,
            ],
        );
        assert_eq!(text(&st), "Xbc\nXef");
        assert!(
            st.register().is_blockwise(),
            "the deleted block is captured"
        );
        assert_eq!(st.register().text(), b"a\nd");
    }

    #[test]
    fn block_append_pads_a_short_lower_row() {
        let st = run(
            "ab\nc",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Blockwise,
                },
                Command::MoveRight,
                Command::MoveDown, // block cols 0..1, rows 0..1
                Command::BlockInsert(BlockInsertKind::Append),
                Command::InsertChar('X'),
                Command::EnterNormal,
            ],
        );
        // Append column is col 2; the short second line "c" is padded with a space to reach it.
        assert_eq!(text(&st), "abX\nc X");
    }

    #[test]
    fn block_insert_with_no_typed_text_is_inert() {
        let st = run(
            "abc\ndef",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Blockwise,
                },
                Command::MoveDown,
                Command::BlockInsert(BlockInsertKind::Insert),
                Command::EnterNormal, // Esc without typing
            ],
        );
        assert_eq!(text(&st), "abc\ndef", "no text typed → no replicate");
        assert_eq!(st.mode(), Mode::Normal);
    }

    #[test]
    fn forced_blockwise_operator_deletes_a_column_across_rows() {
        // `d<C-v>j`: the cursor and the `j` target are the block's corners — one column, two rows.
        let st = run(
            "abc\ndef",
            &[Command::OpForced {
                op: OpKind::Delete,
                count: 1,
                motion: Motion::Down,
                wise: ForcedWise::Blockwise,
            }],
        );
        assert_eq!(text(&st), "bc\nef");
        assert!(st.register().is_blockwise());
        assert_eq!(st.register().text(), b"a\nd");
    }
}

#[cfg(test)]
mod text_object_tests {
    use crate::editor::*;

    fn run(initial: &str, cmds: &[Command]) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        for c in cmds {
            apply_command(&mut st, c);
        }
        st
    }

    fn text(st: &EditorState) -> String {
        String::from_utf8(st.bytes().to_vec()).expect("utf8")
    }

    fn pair(open: char, close: char, around: bool) -> Motion {
        Motion::Pair {
            open,
            close,
            around,
        }
    }

    #[test]
    fn iw_splits_on_punctuation_but_big_word_does_not() {
        // cursor on 'f' of "foo.bar": `iw` is the word class run "foo"; `iW` is the whole WORD "foo.bar".
        let st = run("foo.bar", &[Command::Delete(1, Motion::InnerWord)]);
        assert_eq!(text(&st), ".bar", "diw stops at the punctuation");
        let st = run("foo.bar baz", &[Command::Delete(1, Motion::InnerBigWord)]);
        assert_eq!(text(&st), " baz", "diW spans the punctuation");
    }

    #[test]
    fn aw_and_a_big_word_take_trailing_whitespace() {
        let st = run("foo bar baz", &[Command::Delete(1, Motion::AWord)]);
        assert_eq!(
            text(&st),
            "bar baz",
            "daw removes the word and its trailing space"
        );
        let st = run("foo.bar baz", &[Command::Delete(1, Motion::ABigWord)]);
        assert_eq!(
            text(&st),
            "baz",
            "daW removes the WORD and its trailing space"
        );
    }

    #[test]
    fn delimiter_pair_inner_and_around() {
        // cursor inside the parens of "a(bc)d".
        let st = run(
            "a(bc)d",
            &[
                Command::Move(2, Motion::Right),
                Command::Delete(1, pair('(', ')', false)),
            ],
        );
        assert_eq!(text(&st), "a()d", "di( deletes the interior");
        let st = run(
            "a(bc)d",
            &[
                Command::Move(2, Motion::Right),
                Command::Delete(1, pair('(', ')', true)),
            ],
        );
        assert_eq!(text(&st), "ad", "da( deletes the delimiters too");
    }

    #[test]
    fn delimiter_pair_is_nesting_aware() {
        // On the inner content of "(a(b)c)": from the outer, di( takes everything inside the OUTER pair.
        let st = run("(a(b)c)", &[Command::Delete(1, pair('(', ')', false))]);
        assert_eq!(
            text(&st),
            "()",
            "di( on the opener spans to the matching closer"
        );
        // Cursor inside the inner pair selects only the inner interior.
        let st = run(
            "(a(b)c)",
            &[
                Command::Move(3, Motion::Right),
                Command::Delete(1, pair('(', ')', false)),
            ],
        );
        assert_eq!(
            text(&st),
            "(a()c)",
            "di( from inside the inner pair takes only 'b'"
        );
    }

    #[test]
    fn delimiter_object_outside_a_pair_is_a_noop() {
        let st = run("abc", &[Command::Delete(1, pair('(', ')', false))]);
        assert_eq!(text(&st), "abc");
    }

    #[test]
    fn quote_inner_and_around() {
        // `a "hi" b`: quotes at 2 and 5; cursor on 'h'.
        let st = run(
            "a \"hi\" b",
            &[
                Command::Move(3, Motion::Right),
                Command::Change(
                    1,
                    Motion::Quote {
                        ch: '"',
                        around: false,
                    },
                ),
            ],
        );
        assert_eq!(text(&st), "a \"\" b", "ci\" clears the interior");
        assert_eq!(st.mode(), Mode::Insert);
        let st = run(
            "a \"hi\" b",
            &[
                Command::Move(3, Motion::Right),
                Command::Delete(
                    1,
                    Motion::Quote {
                        ch: '"',
                        around: true,
                    },
                ),
            ],
        );
        assert_eq!(
            text(&st),
            "a b",
            "da\" removes the quotes and the trailing space"
        );
    }

    #[test]
    fn quotes_are_single_line() {
        // The quote on the next line must not pair with one on this line.
        let st = run(
            "x\"a\nb\"y",
            &[Command::Delete(
                1,
                Motion::Quote {
                    ch: '"',
                    around: false,
                },
            )],
        );
        assert_eq!(
            text(&st),
            "x\"a\nb\"y",
            "no matching quote on this line → no-op"
        );
    }

    #[test]
    fn paragraph_inner_and_around() {
        // "l1\nl2\n\nl3\n": cursor in the first paragraph.
        let st = run(
            "l1\nl2\n\nl3\n",
            &[Command::Delete(1, Motion::InnerParagraph)],
        );
        assert_eq!(text(&st), "\nl3\n", "dip removes the paragraph's lines");
        let st = run("l1\nl2\n\nl3\n", &[Command::Delete(1, Motion::AParagraph)]);
        assert_eq!(
            text(&st),
            "l3\n",
            "dap also removes the trailing blank line"
        );
    }

    #[test]
    fn sentence_inner_and_around() {
        let st = run("One. Two.", &[Command::Delete(1, Motion::InnerSentence)]);
        assert_eq!(
            text(&st),
            " Two.",
            "dis removes the first sentence, keeping the space"
        );
        let st = run("One. Two.", &[Command::Delete(1, Motion::ASentence)]);
        assert_eq!(
            text(&st),
            "Two.",
            "das removes the sentence and its trailing space"
        );
    }

    #[test]
    fn text_object_selects_in_visual() {
        // `viw` spans the word under the cursor.
        let st = run(
            "foo bar",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Charwise,
                },
                Command::Move(1, Motion::InnerWord),
            ],
        );
        assert_eq!(
            st.mode(),
            Mode::Visual {
                kind: SelectKind::Charwise
            }
        );
        assert_eq!(st.selection_span(), Some((0, 3)), "viw selects 'foo'");
        // `vi(` spans the interior of the enclosing pair.
        let st = run(
            "a(bc)d",
            &[
                Command::Move(2, Motion::Right),
                Command::EnterVisual {
                    kind: SelectKind::Charwise,
                },
                Command::Move(1, pair('(', ')', false)),
            ],
        );
        assert_eq!(st.selection_span(), Some((2, 4)), "vi( selects 'bc'");
    }
}

#[cfg(test)]
mod select_tests {
    use crate::editor::*;

    fn run(initial: &str, cmds: &[Command]) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        for c in cmds {
            apply_command(&mut st, c);
        }
        st
    }

    fn text(st: &EditorState) -> String {
        String::from_utf8(st.bytes().to_vec()).expect("utf8")
    }

    #[test]
    fn ctrl_g_toggles_visual_to_select_preserving_the_selection() {
        // v + l + l selects "hel"; CTRL-G (EnterSelect) keeps that exact span, now in Select.
        let st = run(
            "hello",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Charwise,
                },
                Command::MoveRight,
                Command::MoveRight,
                Command::EnterSelect {
                    kind: SelectKind::Charwise,
                },
            ],
        );
        assert_eq!(
            st.mode(),
            Mode::Select {
                kind: SelectKind::Charwise
            }
        );
        assert_eq!(
            st.selection_span(),
            Some((0, 3)),
            "selection survives the toggle"
        );
    }

    #[test]
    fn ctrl_g_toggles_select_back_to_visual_preserving_the_selection() {
        let st = run(
            "hello",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Charwise,
                },
                Command::MoveRight,
                Command::EnterSelect {
                    kind: SelectKind::Charwise,
                },
                Command::EnterVisual {
                    kind: SelectKind::Charwise,
                },
            ],
        );
        assert_eq!(
            st.mode(),
            Mode::Visual {
                kind: SelectKind::Charwise
            }
        );
        assert_eq!(
            st.selection_span(),
            Some((0, 2)),
            "toggling back keeps the span"
        );
    }

    #[test]
    fn printable_key_replaces_the_selection_and_enters_insert() {
        // Select "he", then a printable key deletes it, inserts the char, and drops into Insert.
        let st = run(
            "hello",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Charwise,
                },
                Command::MoveRight,
                Command::EnterSelect {
                    kind: SelectKind::Charwise,
                },
                Command::ReplaceSelection('Z'),
            ],
        );
        assert_eq!(text(&st), "Zllo");
        assert_eq!(st.mode(), Mode::Insert);
        assert_eq!(
            st.cursor(),
            1,
            "cursor sits after the inserted char, ready to type"
        );
        assert_eq!(
            st.register().text(),
            b"he",
            "the replaced span fills the register"
        );
        assert_eq!(st.selection_span(), None);
    }

    #[test]
    fn replace_selection_multibyte() {
        let st = run(
            "hello",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Charwise,
                },
                Command::MoveRight,
                Command::EnterSelect {
                    kind: SelectKind::Charwise,
                },
                Command::ReplaceSelection('가'),
            ],
        );
        assert_eq!(text(&st), "가llo");
        assert_eq!(st.mode(), Mode::Insert);
    }

    #[test]
    fn delete_on_a_select_selection_behaves_like_visual() {
        let st = run(
            "hello",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Charwise,
                },
                Command::MoveRight,
                Command::MoveRight,
                Command::EnterSelect {
                    kind: SelectKind::Charwise,
                },
                Command::DeleteSelection,
            ],
        );
        assert_eq!(text(&st), "lo");
        assert_eq!(st.mode(), Mode::Normal);
        assert_eq!(st.register().text(), b"hel");
    }

    #[test]
    fn yank_on_a_select_selection_behaves_like_visual() {
        let st = run(
            "hello",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Charwise,
                },
                Command::MoveRight,
                Command::EnterSelect {
                    kind: SelectKind::Charwise,
                },
                Command::YankSelection,
            ],
        );
        assert_eq!(text(&st), "hello");
        assert_eq!(st.mode(), Mode::Normal);
        assert_eq!(st.register().text(), b"he");
    }

    #[test]
    fn motion_extends_the_select_selection() {
        // A bare motion in Select moves the cursor and keeps the anchor — exactly as in Visual.
        let st = run(
            "hello",
            &[
                Command::EnterSelect {
                    kind: SelectKind::Charwise,
                },
                Command::MoveRight,
                Command::MoveRight,
            ],
        );
        assert_eq!(
            st.mode(),
            Mode::Select {
                kind: SelectKind::Charwise
            }
        );
        assert_eq!(st.selection_span(), Some((0, 3)));
    }

    #[test]
    fn esc_leaves_select_without_editing() {
        let st = run(
            "hello",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Charwise,
                },
                Command::MoveRight,
                Command::EnterSelect {
                    kind: SelectKind::Charwise,
                },
                Command::EnterNormal,
            ],
        );
        assert_eq!(text(&st), "hello");
        assert_eq!(st.mode(), Mode::Normal);
        assert_eq!(st.selection_span(), None);
    }
}

#[cfg(test)]
mod curswant_tests {
    use crate::editor::*;

    fn run(initial: &str, cmds: &[Command]) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        for c in cmds {
            apply_command(&mut st, c);
        }
        st
    }

    // The char column of the cursor on its line.
    fn col(st: &EditorState) -> usize {
        let b = st.bytes();
        motion::col_of(b, motion::line_start(b, st.cursor()), st.cursor())
    }

    #[test]
    fn j_keeps_wanted_column_through_a_short_interior_line() {
        // "abc" col2 -> down onto short "x" (collapses to its end) -> down onto "lmnop": Vim restores the
        // wanted column 2 ('n'), NOT the short line's column. Without curswant it would land on 'm' (col1).
        let st = run(
            "abc\nx\nlmnop\n",
            &[
                Command::Move(2, Motion::Right), // -> 'c' (col 2), curswant = 2
                Command::Move(1, Motion::Down),  // -> short line "x"
                Command::Move(1, Motion::Down),  // -> "lmnop", column restored
            ],
        );
        assert_eq!(col(&st), 2);
        assert_eq!(st.cursor(), 8); // the 'n' in "lmnop"
    }

    #[test]
    fn k_keeps_wanted_column_through_a_short_interior_line() {
        // Symmetric upward: reach the last line's col 3, up through short "x", up to "abcd" col 3 ('d').
        let st = run(
            "abcd\nx\nwxyz\n",
            &[
                Command::Move(1, Motion::Down),  // -> "x"
                Command::Move(1, Motion::Down),  // -> "wxyz" (col 0, curswant still 0)
                Command::Move(3, Motion::Right), // -> 'z' (col 3), curswant = 3
                Command::Move(1, Motion::Up),    // -> short "x"
                Command::Move(1, Motion::Up),    // -> "abcd", column 3 restored
            ],
        );
        assert_eq!(col(&st), 3);
        assert_eq!(st.cursor(), 3); // the 'd' in "abcd"
    }

    #[test]
    fn dollar_makes_the_column_ride_each_line_end() {
        // `$` sets curswant = MAXCOL, so successive `j` stay on each line's LAST char (not a fixed column),
        // and Normal mode never rests on the trailing newline.
        let st = run(
            "ab\nwxyz\nc\n",
            &[
                Command::Move(1, Motion::LineEnd), // `$` on "ab" -> 'b', curswant = MAXCOL
                Command::Move(1, Motion::Down),    // -> "wxyz" last char 'z'
            ],
        );
        assert_eq!(st.cursor(), 6); // the 'z' in "wxyz" (not past it)
        let st2 = run(
            "ab\nwxyz\nc\n",
            &[
                Command::Move(1, Motion::LineEnd),
                Command::Move(2, Motion::Down), // ride down two lines to short "c"
            ],
        );
        assert_eq!(st2.cursor(), 8); // the 'c' (its only char)
    }

    #[test]
    fn a_horizontal_move_resets_the_wanted_column() {
        // After `$` (MAXCOL), an `h` resets curswant to the actual column, so a later `j` no longer rides ends.
        let st = run(
            "abcd\nwx\nlmnop\n",
            &[
                Command::Move(1, Motion::LineEnd), // 'd' (col 3), curswant = MAXCOL
                Command::Move(1, Motion::Left),    // 'c' (col 2), curswant reset to 2
                Command::Move(1, Motion::Down),    // "wx" -> clamps to col 1 ('x'), curswant kept 2
                Command::Move(1, Motion::Down),    // "lmnop" col 2 ('n'), NOT the end
            ],
        );
        assert_eq!(st.cursor(), 10); // the 'n' in "lmnop" (col 2), proving curswant = 2 not MAXCOL
    }

    fn text(st: &EditorState) -> String {
        String::from_utf8(st.bytes().to_vec()).expect("utf8")
    }

    // Insert-mode append column (Vim i_CTRL-O at EOL). A one-shot CTRL-O routes the next Normal command
    // through the engine while core mode STAYS Insert, so at the core level it is just that Normal command
    // executed in Insert mode; these tests drive that command sequence directly.

    #[test]
    fn ctrl_o_dollar_appends_at_end_of_line() {
        // `i<C-o>$X`: `$` sets curswant = MAXCOL, which in Insert parks the caret at the append position, so
        // `X` lands at the END of the line, not on the last char.
        let st = run(
            "hi",
            &[
                Command::EnterInsert,
                Command::Move(1, Motion::LineEnd), // the one-shot `$`
                Command::InsertChar('X'),
                Command::EnterNormal,
            ],
        );
        assert_eq!(text(&st), "hiX");
        assert_eq!(st.cursor(), 2); // on 'X' after the Esc nudge
    }

    #[test]
    fn ctrl_o_dd_preserves_the_append_intent() {
        // `A<C-o>ddX`: `A` sets the append intent (curswant = MAXCOL); the one-shot `dd` PRESERVES it (an
        // edit, not a column-setting move), so insert resumes at the end of the line dd left behind and `X`
        // appends there → "betaX".
        let st = run(
            "alpha\nbeta",
            &[
                Command::AppendLineEnd, // append at end of "alpha", curswant = MAXCOL
                Command::Delete(1, Motion::Line), // the one-shot `dd` -> buffer "beta"
                Command::InsertChar('X'),
                Command::EnterNormal,
            ],
        );
        assert_eq!(text(&st), "betaX");
        assert_eq!(st.cursor(), 4); // on 'X' (col 4 of "betaX")
    }

    #[test]
    fn ctrl_o_line_start_resets_the_append_intent() {
        // `A<C-o>0X`: `A` sets MAXCOL but the one-shot `0` is a column-setting move that resets it, so `X`
        // inserts at column 0 → "Xalpha".
        let st = run(
            "alpha\nbeta",
            &[
                Command::AppendLineEnd,
                Command::Move(1, Motion::LineStart), // the one-shot `0`
                Command::InsertChar('X'),
                Command::EnterNormal,
            ],
        );
        assert_eq!(text(&st), "Xalpha\nbeta");
        assert_eq!(st.cursor(), 0);
    }
}

#[cfg(test)]
mod virtual_replace_tests {
    use crate::editor::*;

    // Virtual Replace (`gR`) with tab_width = 4 (the EditorState default).
    fn run(initial: &str, cmds: &[Command]) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        for c in cmds {
            apply_command(&mut st, c);
        }
        st
    }

    fn text(st: &EditorState) -> String {
        String::from_utf8(st.bytes().to_vec()).expect("utf8")
    }

    #[test]
    fn without_tabs_it_behaves_like_replace() {
        let st = run(
            "hello",
            &[
                Command::EnterVirtualReplace,
                Command::VirtualReplaceType('x'),
                Command::VirtualReplaceType('y'),
                Command::EnterNormal,
            ],
        );
        assert_eq!(text(&st), "xyllo");
    }

    #[test]
    fn typing_over_a_tab_inserts_before_it_and_shrinks_it() {
        // "a<Tab>b" (ts=4): the tab spans virtual cols 1..3. Typing 2 chars over it keeps the tab (now 1
        // column wide) and inserts before it: "aXY<Tab>b".
        let st = run(
            "a\tb",
            &[
                Command::Move(1, Motion::Right), // onto the tab
                Command::EnterVirtualReplace,
                Command::VirtualReplaceType('X'),
                Command::VirtualReplaceType('Y'),
                Command::EnterNormal,
            ],
        );
        assert_eq!(text(&st), "aXY\tb");
    }

    #[test]
    fn consuming_the_tabs_last_column_replaces_it() {
        // A third char eats the tab's last virtual column, so the tab is removed: "aXYZb".
        let st = run(
            "a\tb",
            &[
                Command::Move(1, Motion::Right),
                Command::EnterVirtualReplace,
                Command::VirtualReplaceType('X'),
                Command::VirtualReplaceType('Y'),
                Command::VirtualReplaceType('Z'),
                Command::EnterNormal,
            ],
        );
        assert_eq!(text(&st), "aXYZb");
    }

    #[test]
    fn backspace_over_a_shrunk_tab_regrows_it() {
        // Typing one char over the tab then <BS> deletes the inserted char, restoring the original tab.
        let st = run(
            "a\tb",
            &[
                Command::Move(1, Motion::Right),
                Command::EnterVirtualReplace,
                Command::VirtualReplaceType('X'),
                Command::ReplaceBackspace,
                Command::EnterNormal,
            ],
        );
        assert_eq!(text(&st), "a\tb");
    }
}

#[cfg(test)]
mod search_tests {
    use crate::command::SearchOp;
    use crate::editor::*;

    fn run(initial: &str, cmds: &[Command]) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        for c in cmds {
            apply_command(&mut st, c);
        }
        st
    }

    fn search(pattern: &str) -> Command {
        Command::Search {
            op: SearchOp::Move,
            count: 1,
            pattern: pattern.to_string(),
        }
    }

    /// F-009 #3: `/pat` is a Vim-REGEX search now (not literal). `a\+` matches one-or-more `a`; the
    /// cursor lands on the start of the next match.
    #[test]
    fn slash_search_uses_the_vim_regex_engine() {
        // `a\+` is a Vim-regex quantifier (magic `\+`), not the literal "a\+".
        let st = run("x aa bbb aaa", &[search("a\\+")]);
        // From offset 0, the next match after cur+1 is the "aa" at offset 2.
        assert_eq!(st.cursor(), 2);
        // `n` (SearchNext) steps to the following, non-overlapping match ("cc" at offset 9).
        let st = run(
            "x aa bbb cc",
            &[search("cc"), Command::SearchNext("cc".into())],
        );
        // Single `cc` match at 9; `n` wraps back to it.
        assert_eq!(st.cursor(), 9);
    }

    /// F-009 #3: `\zs` moves the reported match START — `/foo\zsbar` lands the cursor on `bar`, but
    /// only where `bar` follows `foo`.
    #[test]
    fn zs_lands_the_cursor_at_the_reset_start() {
        let st = run("a foobar foo bar", &[search("foo\\zsbar")]);
        assert_eq!(st.cursor(), 5); // the 'b' of the bar inside "foobar"
    }

    /// F-009 #1: search is CASE-SENSITIVE by default (Vim's factory setting, what the oracle runs), so
    /// `/Foo` does not match `foo`.
    #[test]
    fn search_is_case_sensitive_by_default() {
        let st = run("a foo b", &[search("Foo")]);
        assert_eq!(st.cursor(), 0, "no match → cursor unmoved");
    }

    /// F-009 #1: with `smartcase` on, an all-lowercase pattern matches case-insensitively, but an
    /// uppercase in the pattern forces a case-sensitive match (case-smart search).
    #[test]
    fn smartcase_makes_search_case_smart() {
        // lowercase pattern → case-insensitive → matches "FOO"
        let mut st = EditorState::new(b"x FOO y".to_vec());
        st.set_search_case(true, true);
        apply_command(&mut st, &search("foo"));
        assert_eq!(st.cursor(), 2);
        // uppercase in the pattern → case-sensitive → "Foo" does NOT match "foo"
        let mut st = EditorState::new(b"x foo y".to_vec());
        st.set_search_case(true, true);
        apply_command(&mut st, &search("Foo"));
        assert_eq!(st.cursor(), 0, "smartcase forced case-sensitive → no match");
    }

    /// F-009: `d/pat` deletes `[cursor, match)` — the search operand still composes with operators.
    #[test]
    fn delete_to_search_match() {
        let st = run(
            "abcXYZdef",
            &[Command::Search {
                op: SearchOp::Delete,
                count: 1,
                pattern: "XYZ".into(),
            }],
        );
        assert_eq!(st.as_str().unwrap(), "XYZdef");
    }

    fn gn(op: SearchOp, count: u32, pattern: &str, backward: bool) -> Command {
        Command::SearchObject {
            op,
            count,
            pattern: pattern.to_string(),
            backward,
        }
    }

    #[test]
    fn gn_selects_the_next_match_in_visual() {
        // Cursor at 0 (before any match); `gn` selects the first "foo" as a charwise Visual span.
        let st = run("x foo y foo", &[gn(SearchOp::Move, 1, "foo", false)]);
        assert_eq!(
            st.mode(),
            Mode::Visual {
                kind: SelectKind::Charwise
            }
        );
        assert_eq!(
            st.selection_span(),
            Some((2, 5)),
            "selects the first foo [2,5)"
        );
    }

    #[test]
    fn gn_selects_the_match_under_the_cursor() {
        // Cursor INSIDE the first match (on the 'o' at byte 3) selects THAT match, not the next.
        let st = run(
            "x foo y foo",
            &[
                Command::Move(3, Motion::Right),
                gn(SearchOp::Move, 1, "foo", false),
            ],
        );
        assert_eq!(
            st.selection_span(),
            Some((2, 5)),
            "the containing match, not the next"
        );
    }

    #[test]
    fn dgn_deletes_the_next_whole_match() {
        let st = run("x foo y", &[gn(SearchOp::Delete, 1, "foo", false)]);
        assert_eq!(st.as_str().unwrap(), "x  y");
        assert_eq!(st.mode(), Mode::Normal);
    }

    #[test]
    fn cgn_deletes_the_match_and_enters_insert() {
        let st = run("x foo y", &[gn(SearchOp::Change, 1, "foo", false)]);
        assert_eq!(st.as_str().unwrap(), "x  y");
        assert_eq!(st.mode(), Mode::Insert);
        assert_eq!(
            st.cursor(),
            2,
            "cursor at the match start, ready to type the replacement"
        );
    }

    #[test]
    fn count_gn_reaches_a_later_match() {
        // `2gn` from the top selects the SECOND match.
        let st = run("a b a b a", &[gn(SearchOp::Move, 2, "a", false)]);
        assert_eq!(st.selection_span(), Some((4, 5)), "the 2nd 'a'");
    }

    #[test]
    fn gn_backward_selects_the_previous_match() {
        // Cursor past the last match; `gN` walks backward to it.
        let st = run(
            "foo bar foo",
            &[
                Command::Move(100, Motion::Right),
                gn(SearchOp::Move, 1, "foo", true),
            ],
        );
        assert_eq!(
            st.selection_span(),
            Some((8, 11)),
            "the last foo, selected backward"
        );
    }

    #[test]
    fn gn_with_no_match_is_a_noop() {
        let st = run("hello", &[gn(SearchOp::Move, 1, "zzz", false)]);
        assert_eq!(st.mode(), Mode::Normal, "no match — stays in Normal");
        assert_eq!(st.cursor(), 0);
    }
}

#[cfg(test)]
mod substitute_tests {
    use crate::editor::*;

    fn sub(
        initial: &str,
        range: SubRange,
        pat: &str,
        rep: &str,
        flags: SubFlags,
    ) -> (EditorState, SubOutcome) {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        let out = st.substitute(range, pat, rep, flags).expect("compiles");
        (st, out)
    }

    /// F-009 #2: `:s/pat/rep/` replaces the FIRST match on the current line only.
    #[test]
    fn substitute_first_match_on_current_line() {
        let (st, out) = sub(
            "foo foo\nfoo",
            SubRange::CurrentLine,
            "foo",
            "bar",
            SubFlags::default(),
        );
        assert_eq!(st.as_str().unwrap(), "bar foo\nfoo");
        assert_eq!((out.replacements, out.lines), (1, 1));
    }

    /// F-009 #2: the `g` flag replaces ALL matches on each line.
    #[test]
    fn substitute_global_flag() {
        let flags = SubFlags {
            global: true,
            ignore_case: None,
        };
        let (st, out) = sub("foo foo foo", SubRange::CurrentLine, "foo", "X", flags);
        assert_eq!(st.as_str().unwrap(), "X X X");
        assert_eq!(out.replacements, 3);
    }

    /// F-009 #2: `:%s///g` spans the whole file as ONE undo group.
    #[test]
    fn substitute_whole_file_is_one_undo_group() {
        let flags = SubFlags {
            global: true,
            ignore_case: None,
        };
        let (mut st, _) = sub("a a\nb a\na", SubRange::WholeFile, "a", "Z", flags);
        assert_eq!(st.as_str().unwrap(), "Z Z\nb Z\nZ");
        // A single `u` reverts the ENTIRE substitution (it was one transaction).
        apply_command(&mut st, &Command::Undo);
        assert_eq!(st.as_str().unwrap(), "a a\nb a\na");
    }

    /// F-009 #2: a `:N,Ms` line range restricts the substitution.
    #[test]
    fn substitute_line_range() {
        let flags = SubFlags {
            global: true,
            ignore_case: None,
        };
        let (st, _) = sub("a\na\na\na", SubRange::Lines(2, 3), "a", "X", flags);
        assert_eq!(st.as_str().unwrap(), "a\nX\nX\na");
    }

    /// F-009 #2: `&` in the replacement is the whole matched text (`:s/foo/[&]/`).
    #[test]
    fn ampersand_is_the_whole_match() {
        let (st, _) = sub(
            "say foo",
            SubRange::CurrentLine,
            "foo",
            "[&]",
            SubFlags::default(),
        );
        assert_eq!(st.as_str().unwrap(), "say [foo]");
    }

    /// F-009 #3: `:s/foo\zsbar/X/` rewrites only the reported (`\zs`) span — `foo` is kept.
    #[test]
    fn substitute_honors_zs_reported_span() {
        let (st, _) = sub(
            "a foobar b",
            SubRange::CurrentLine,
            "foo\\zsbar",
            "X",
            SubFlags::default(),
        );
        assert_eq!(st.as_str().unwrap(), "a fooX b");
    }

    /// F-009 #2: the `i` flag forces a case-insensitive substitution regardless of config.
    #[test]
    fn substitute_i_flag_ignores_case() {
        let flags = SubFlags {
            global: true,
            ignore_case: Some(true),
        };
        let (st, _) = sub("Foo FOO foo", SubRange::CurrentLine, "foo", "x", flags);
        assert_eq!(st.as_str().unwrap(), "x x x");
    }
}

#[cfg(test)]
mod substitute_confirm_tests {
    use crate::editor::*;

    /// F-009 #2 (`:s///c`): preview finds all candidate matches WITHOUT editing; applying only an
    /// ACCEPTED subset (as the confirm loop does on y/n) replaces just those, in one undo group.
    #[test]
    fn preview_then_apply_accepted_subset() {
        let flags = SubFlags {
            global: true,
            ignore_case: None,
        };
        let mut st = EditorState::new(b"a a a".to_vec());
        let subs = st
            .substitute_preview(SubRange::CurrentLine, "a", "X", flags)
            .expect("compiles");
        assert_eq!(subs.len(), 3, "three candidate matches");
        assert_eq!(st.as_str().unwrap(), "a a a", "preview does not edit");

        // Accept the first and last, skip the middle (as `y n y` would).
        let accepted = vec![subs[0].clone(), subs[2].clone()];
        let out = st.apply_substitutions(&accepted);
        assert_eq!(st.as_str().unwrap(), "X a X");
        assert_eq!(out.replacements, 2);
        // One `u` reverts BOTH accepted substitutions (they were one transaction).
        apply_command(&mut st, &Command::Undo);
        assert_eq!(st.as_str().unwrap(), "a a a");
    }

    /// Applying an EMPTY accepted set (the confirm loop when the user answered `n` to everything or
    /// quit immediately) is a clean no-op.
    #[test]
    fn apply_no_substitutions_is_a_noop() {
        let mut st = EditorState::new(b"a a".to_vec());
        let out = st.apply_substitutions(&[]);
        assert_eq!((out.replacements, out.lines), (0, 0));
        assert_eq!(st.as_str().unwrap(), "a a");
    }
}

#[cfg(test)]
mod global_tests {
    use crate::editor::*;

    fn run_global(initial: &str, pat: &str, negate: bool, cmd: GlobalCmd) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        st.global(SubRange::WholeFile, pat, negate, &cmd)
            .expect("compiles");
        st
    }

    /// F-009 #4: `:g/pat/d` deletes every line matching the pattern (two-pass: mark then execute).
    #[test]
    fn global_delete_matching_lines() {
        let st = run_global(
            "keep\ndrop me\nkeep2\ndrop\n",
            "drop",
            false,
            GlobalCmd::Delete,
        );
        assert_eq!(st.as_str().unwrap(), "keep\nkeep2\n");
    }

    /// F-009 #4: `:g!/pat/d` (== `:v/pat/d`) deletes the NON-matching lines.
    #[test]
    fn global_negate_deletes_non_matching() {
        let st = run_global("a1\nb\na2\nc\n", "a", true, GlobalCmd::Delete);
        assert_eq!(st.as_str().unwrap(), "a1\na2\n");
    }

    /// F-009 #4: the two-pass mark-then-execute means a `:g/pat/s///` only substitutes on lines that
    /// matched the `:g` selector, and marking is unaffected by the substitutions.
    #[test]
    fn global_substitute_on_matching_lines_only() {
        let st = run_global(
            "foo x\nbar x\nfoo x\n",
            "foo",
            false,
            GlobalCmd::Substitute {
                pattern: "x".into(),
                replacement: "Y".into(),
                flags: SubFlags {
                    global: true,
                    ignore_case: None,
                },
            },
        );
        // Only the two "foo" lines get their x -> Y; the "bar" line is untouched.
        assert_eq!(st.as_str().unwrap(), "foo Y\nbar x\nfoo Y\n");
    }

    /// F-009 #4: the whole `:g` is ONE undo group.
    #[test]
    fn global_delete_is_one_undo_group() {
        let mut st = EditorState::new(b"a\ndrop\nb\ndrop\nc\n".to_vec());
        st.global(SubRange::WholeFile, "drop", false, &GlobalCmd::Delete)
            .unwrap();
        assert_eq!(st.as_str().unwrap(), "a\nb\nc\n");
        apply_command(&mut st, &Command::Undo);
        assert_eq!(st.as_str().unwrap(), "a\ndrop\nb\ndrop\nc\n");
    }
}

#[cfg(test)]
mod mark_tests {
    use crate::editor::*;

    fn run(initial: &str, cmds: &[Command]) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        for c in cmds {
            apply_command(&mut st, c);
        }
        st
    }

    fn text(st: &EditorState) -> String {
        String::from_utf8(st.bytes().to_vec()).expect("utf8")
    }

    // Emacs `delete-char` (D-026): deletes forward WITHOUT filling the kill ring (unlike Vim `x`), and it
    // crosses the newline (Emacs has no end-of-line boundary).
    #[test]
    fn delete_forward_does_not_yank_and_crosses_newline() {
        let st = run("hello", &[Command::DeleteForward(1)]);
        assert_eq!(text(&st), "ello");
        assert_eq!(st.view.cursor, 0);
        assert!(
            st.register().is_empty(),
            "delete-char must not write the kill ring"
        );

        // On the newline it deletes forward across it, joining the lines (buffer-end clamp, not EOL).
        let mut joined = EditorState::new(b"ab\ncd".to_vec());
        joined.set_cursor(2); // on the '\n'
        apply_command(&mut joined, &Command::DeleteForward(1));
        assert_eq!(text(&joined), "abcd");
        assert!(joined.register().is_empty());
    }

    // C-SPC then move then C-w: the region [mark, point) is killed into the register; Emacs leaves point AND
    // mark together at the region's lower bound (D-050 Family 3 — kill-region keeps the mark, not clears it).
    #[test]
    fn set_mark_then_kill_region_deletes_and_fills_register() {
        let st = run(
            "hello world\n",
            &[
                Command::SetMark,
                Command::MoveRight,
                Command::MoveRight,
                Command::MoveRight,
                Command::MoveRight,
                Command::MoveRight,
                Command::KillRegion,
            ],
        );
        assert_eq!(text(&st), " world\n");
        assert_eq!(st.view.cursor, 0);
        assert_eq!(
            st.view.mark,
            Some(0),
            "kill-region leaves the mark at the region start"
        );
    }

    // Emacs yank (D-051): pastes the register at point AND sets the mark at the insertion start, leaving
    // point after the pasted text (between-char). Distinct from Vim `P`, which never touches the mark.
    #[test]
    fn emacs_yank_sets_mark_at_insertion_start() {
        let mut st = EditorState::new(b"abc".to_vec());
        st.set_caret_gravity(CaretGravity::BetweenChar);
        apply_command(&mut st, &Command::Yank(1, Motion::LineEnd)); // register := "abc", point stays 0
        apply_command(&mut st, &Command::MoveLineEnd); // Emacs move-end-of-line → point 3 (after "abc")
        apply_command(&mut st, &Command::EmacsYank { count: 1 });
        assert_eq!(text(&st), "abcabc");
        assert_eq!(st.view.cursor, 6, "point rests after the pasted text");
        assert_eq!(
            st.view.mark,
            Some(3),
            "yank sets the mark at the insertion start"
        );
    }

    // Emacs beginning/end-of-buffer (D-051): jump to the ABSOLUTE buffer edge and push the mark at the old
    // point — not Vim `gg`/`G` (first-non-blank of a line).
    #[test]
    fn emacs_buffer_edges_jump_absolute_and_push_mark() {
        let end = run(
            "  alpha\nbeta",
            &[Command::EmacsBufferEdge { start: false }],
        );
        assert_eq!(
            end.view.cursor, 12,
            "end-of-buffer lands at the absolute end (buffer length)"
        );
        assert_eq!(
            end.view.mark,
            Some(0),
            "it pushes the mark at the old point"
        );

        let mut back = EditorState::new(b"  alpha\nbeta".to_vec());
        back.set_cursor(10); // somewhere in "beta"
        apply_command(&mut back, &Command::EmacsBufferEdge { start: true });
        assert_eq!(
            back.view.cursor, 0,
            "beginning-of-buffer lands at absolute 0, not first-non-blank"
        );
        assert_eq!(back.view.mark, Some(10));
    }

    // Emacs kill-line (D-051): kill to EOL into the register, but AT the EOL kill the newline instead
    // (joining the next line), and at end-of-buffer do nothing. Distinct from Vim `D` (which no-ops at EOL).
    #[test]
    fn emacs_kill_line_kills_to_eol_then_joins_at_eol() {
        // Point before the line end: kill the line's rest (not the newline).
        let mut mid = EditorState::new(b"foo\nbar".to_vec());
        mid.set_caret_gravity(CaretGravity::BetweenChar);
        mid.set_cursor(0);
        apply_command(&mut mid, &Command::EmacsKillLine);
        assert_eq!(text(&mid), "\nbar");
        assert_eq!(mid.view.cursor, 0);
        assert_eq!(mid.register().text(), b"foo");

        // A second kill-line, now resting AT the (empty first) line's end: kill the newline, joining "bar".
        apply_command(&mut mid, &Command::EmacsKillLine);
        assert_eq!(text(&mid), "bar");
        assert_eq!(mid.view.cursor, 0);
        // The two consecutive kills ACCUMULATE onto one entry (Emacs kill-ring): "foo" then "\n" -> "foo\n".
        assert_eq!(mid.register().text(), b"foo\n");

        // At end-of-buffer there is nothing to kill: inert, register untouched.
        let mut eob = EditorState::new(b"foo".to_vec());
        eob.set_caret_gravity(CaretGravity::BetweenChar);
        eob.set_cursor(3);
        apply_command(&mut eob, &Command::EmacsKillLine);
        assert_eq!(text(&eob), "foo");
        assert_eq!(eob.view.cursor, 3);
        assert!(eob.register().is_empty());
    }

    // Emacs transpose-chars (D-051): swap the two chars around point and advance; at EOL transpose the two
    // chars ending the line; inert with no pair. It never writes the kill ring.
    #[test]
    fn emacs_transpose_chars_swaps_around_point() {
        // Mid-line: "abc" point 1 -> "bac" point 2 (swap 'a' and 'b').
        let mut mid = EditorState::new(b"abc".to_vec());
        mid.set_caret_gravity(CaretGravity::BetweenChar);
        mid.set_cursor(1);
        apply_command(&mut mid, &Command::EmacsTransposeChars);
        assert_eq!(text(&mid), "bac");
        assert_eq!(mid.view.cursor, 2);
        assert!(
            mid.register().is_empty(),
            "transpose does not touch the kill ring"
        );

        // At end of line: "abc" point 3 -> transpose the last two chars -> "acb", point stays at 3.
        let mut eol = EditorState::new(b"abc".to_vec());
        eol.set_caret_gravity(CaretGravity::BetweenChar);
        eol.set_cursor(3);
        apply_command(&mut eol, &Command::EmacsTransposeChars);
        assert_eq!(text(&eol), "acb");
        assert_eq!(eol.view.cursor, 3);

        // No pair (point at buffer start): inert.
        let mut bob = EditorState::new(b"abc".to_vec());
        bob.set_caret_gravity(CaretGravity::BetweenChar);
        bob.set_cursor(0);
        apply_command(&mut bob, &Command::EmacsTransposeChars);
        assert_eq!(text(&bob), "abc");
        assert_eq!(bob.view.cursor, 0);
    }

    // Emacs case-word family (D-051): recase the forward-word span and leave point at the word end. Never
    // touches the kill ring.
    #[test]
    fn emacs_case_word_recases_the_word_ahead() {
        // capitalize-word: "foo bar" point 0 -> "Foo bar", point at end of "foo" (3).
        let mut cap = EditorState::new(b"foo bar".to_vec());
        cap.set_caret_gravity(CaretGravity::BetweenChar);
        cap.set_cursor(0);
        apply_command(
            &mut cap,
            &Command::EmacsCaseWord {
                case: WordCase::Capitalize,
            },
        );
        assert_eq!(text(&cap), "Foo bar");
        assert_eq!(cap.view.cursor, 3);
        assert!(
            cap.register().is_empty(),
            "case-word does not touch the kill ring"
        );

        // upcase-word: mixed case -> all upper.
        let mut up = EditorState::new(b"fooBar baz".to_vec());
        up.set_caret_gravity(CaretGravity::BetweenChar);
        up.set_cursor(0);
        apply_command(
            &mut up,
            &Command::EmacsCaseWord {
                case: WordCase::Upcase,
            },
        );
        assert_eq!(text(&up), "FOOBAR baz");
        assert_eq!(up.view.cursor, 6);

        // downcase-word skips leading non-word chars (the forward-word span): point before "  FOO".
        let mut down = EditorState::new(b"  FOO".to_vec());
        down.set_caret_gravity(CaretGravity::BetweenChar);
        down.set_cursor(0);
        apply_command(
            &mut down,
            &Command::EmacsCaseWord {
                case: WordCase::Downcase,
            },
        );
        assert_eq!(text(&down), "  foo");
        assert_eq!(down.view.cursor, 5);

        // No word ahead: inert.
        let mut none = EditorState::new(b"foo".to_vec());
        none.set_caret_gravity(CaretGravity::BetweenChar);
        none.set_cursor(3);
        apply_command(
            &mut none,
            &Command::EmacsCaseWord {
                case: WordCase::Upcase,
            },
        );
        assert_eq!(text(&none), "foo");
        assert_eq!(none.view.cursor, 3);
    }

    // Emacs kill-accumulation (D-051): consecutive kills append onto one unnamed entry; a non-kill command
    // between them breaks the run so the next kill starts fresh. Mirrors Emacs `last-command == kill-region`.
    #[test]
    fn emacs_kills_accumulate_and_break_on_non_kill() {
        // Two kill-lines: "foo" then the joining "\n" append -> "foo\n".
        let mut kl = EditorState::new(b"foo\nbar".to_vec());
        kl.set_caret_gravity(CaretGravity::BetweenChar);
        kl.set_cursor(0);
        apply_command(&mut kl, &Command::EmacsKillLine);
        apply_command(&mut kl, &Command::EmacsKillLine);
        assert_eq!(text(&kl), "bar");
        assert_eq!(
            kl.register().text(),
            b"foo\n",
            "consecutive kill-lines accumulate"
        );

        // Two kill-words accumulate: "foo" then " bar" -> "foo bar".
        let mut kw = EditorState::new(b"foo bar".to_vec());
        kw.set_caret_gravity(CaretGravity::BetweenChar);
        kw.set_cursor(0);
        apply_command(&mut kw, &Command::EmacsKillWord { count: 1 });
        apply_command(&mut kw, &Command::EmacsKillWord { count: 1 });
        assert_eq!(text(&kw), "");
        assert_eq!(
            kw.register().text(),
            b"foo bar",
            "consecutive kill-words accumulate"
        );

        // A non-kill (a plain move) between kills BREAKS the run: the second kill overwrites, not appends.
        let mut brk = EditorState::new(b"foo bar baz".to_vec());
        brk.set_caret_gravity(CaretGravity::BetweenChar);
        brk.set_cursor(0);
        apply_command(&mut brk, &Command::EmacsKillWord { count: 1 });
        apply_command(&mut brk, &Command::MoveRight); // not a kill -> resets last_was_kill
        apply_command(&mut brk, &Command::EmacsKillWord { count: 1 });
        assert_eq!(
            brk.register().text(),
            b"bar",
            "a non-kill breaks accumulation"
        );

        // A Vim delete must NEVER accumulate, even back-to-back (accumulation is Emacs-kill-only).
        let mut vim = EditorState::new(b"foo bar".to_vec());
        apply_command(&mut vim, &Command::Delete(1, Motion::WordFwd));
        apply_command(&mut vim, &Command::Delete(1, Motion::WordFwd));
        assert_eq!(
            vim.register().text(),
            b"bar",
            "Vim dw overwrites the register; it never accumulates"
        );
    }

    // Emacs back-to-indentation (M-m) / Vim `^` share Motion::LineFirstNonBlank: land on the first
    // non-blank of the line, or the line end when it is all blank.
    #[test]
    fn line_first_non_blank_lands_on_first_non_blank() {
        let mut st = EditorState::new(b"  foo".to_vec());
        st.set_caret_gravity(CaretGravity::BetweenChar);
        st.set_cursor(5); // end of line
        apply_command(&mut st, &Command::Move(1, Motion::LineFirstNonBlank));
        assert_eq!(
            st.view.cursor, 2,
            "lands on 'f' past the two leading spaces"
        );

        // An all-blank line: land at the line end (no non-blank to find).
        let mut blank = EditorState::new(b"   \nx".to_vec());
        blank.set_caret_gravity(CaretGravity::BetweenChar);
        blank.set_cursor(0);
        apply_command(&mut blank, &Command::Move(1, Motion::LineFirstNonBlank));
        assert_eq!(blank.view.cursor, 3, "all-blank line: land at its end");
    }

    // Emacs just-one-space (M-SPC) / delete-horizontal-space (M-\) (D-051): collapse surrounding spaces/tabs.
    #[test]
    fn emacs_horizontal_space_collapses_surrounding_whitespace() {
        let mut one = EditorState::new(b"foo   bar".to_vec());
        one.set_caret_gravity(CaretGravity::BetweenChar);
        one.set_cursor(4); // inside the three spaces
        apply_command(&mut one, &Command::EmacsHorizontalSpace { keep_one: true });
        assert_eq!(text(&one), "foo bar");
        assert_eq!(one.view.cursor, 4, "point rests after the single space");

        let mut none = EditorState::new(b"foo   bar".to_vec());
        none.set_caret_gravity(CaretGravity::BetweenChar);
        none.set_cursor(4);
        apply_command(
            &mut none,
            &Command::EmacsHorizontalSpace { keep_one: false },
        );
        assert_eq!(text(&none), "foobar");
        assert_eq!(none.view.cursor, 3);

        // just-one-space with no surrounding whitespace inserts one.
        let mut ins = EditorState::new(b"ab".to_vec());
        ins.set_caret_gravity(CaretGravity::BetweenChar);
        ins.set_cursor(1);
        apply_command(&mut ins, &Command::EmacsHorizontalSpace { keep_one: true });
        assert_eq!(text(&ins), "a b");
        assert_eq!(ins.view.cursor, 2);
    }

    // Emacs open-line (C-o) (D-051): insert a newline at point, leaving point before it. No mode change.
    #[test]
    fn emacs_open_line_inserts_newline_and_keeps_point() {
        let mut st = EditorState::new(b"foobar".to_vec());
        st.set_caret_gravity(CaretGravity::BetweenChar);
        st.set_cursor(3);
        apply_command(&mut st, &Command::EmacsOpenLine);
        assert_eq!(text(&st), "foo\nbar");
        assert_eq!(st.view.cursor, 3, "point stays before the inserted newline");
        assert!(st.register().is_empty());
    }

    // Emacs backward-kill-word (M-DEL) (D-051): kill the previous word; consecutive backward kills PREPEND
    // onto the current entry (the backward-kill accumulation direction).
    #[test]
    fn emacs_backward_kill_word_kills_backward_and_prepends() {
        let mut one = EditorState::new(b"foo bar".to_vec());
        one.set_caret_gravity(CaretGravity::BetweenChar);
        one.set_cursor(7);
        apply_command(&mut one, &Command::EmacsBackwardKillWord { count: 1 });
        assert_eq!(text(&one), "foo ");
        assert_eq!(one.view.cursor, 4);
        assert_eq!(one.register().text(), b"bar");

        // Two backward kills: the second PREPENDS ("bar " before "baz" -> "bar baz").
        let mut acc = EditorState::new(b"foo bar baz".to_vec());
        acc.set_caret_gravity(CaretGravity::BetweenChar);
        acc.set_cursor(11);
        apply_command(&mut acc, &Command::EmacsBackwardKillWord { count: 1 });
        apply_command(&mut acc, &Command::EmacsBackwardKillWord { count: 1 });
        assert_eq!(text(&acc), "foo ");
        assert_eq!(acc.register().text(), b"bar baz", "backward kills prepend");
    }

    // Emacs transpose-words (M-t) (D-051): swap the word around point with the following word, keeping the
    // separator; point lands past the moved second word. Verified against the oracle at several positions.
    #[test]
    fn emacs_transpose_words_swaps_adjacent_words() {
        // At the separator between the two words.
        let mut a = EditorState::new(b"foo bar".to_vec());
        a.set_caret_gravity(CaretGravity::BetweenChar);
        a.set_cursor(3);
        apply_command(&mut a, &Command::EmacsTransposeWords);
        assert_eq!(text(&a), "bar foo");
        assert_eq!(a.view.cursor, 7);
        assert!(a.register().is_empty());

        // Point inside the middle word of three swaps that word with the NEXT one (matches Emacs).
        let mut b = EditorState::new(b"foo bar baz".to_vec());
        b.set_caret_gravity(CaretGravity::BetweenChar);
        b.set_cursor(5); // inside "bar"
        apply_command(&mut b, &Command::EmacsTransposeWords);
        assert_eq!(text(&b), "foo baz bar");
        assert_eq!(b.view.cursor, 11);
    }

    // Emacs mark-word (M-@) (D-051): set the mark at the end of the next word, point unchanged.
    #[test]
    fn emacs_mark_word_sets_mark_at_word_end() {
        let mut st = EditorState::new(b"foo bar".to_vec());
        st.set_caret_gravity(CaretGravity::BetweenChar);
        st.set_cursor(0);
        apply_command(&mut st, &Command::EmacsMarkWord);
        assert_eq!(st.view.cursor, 0, "point does not move");
        assert_eq!(st.mark(), Some(3), "mark lands at the end of 'foo'");
        assert!(st.register().is_empty());
    }

    // Emacs kill-whole-line (C-S-DEL) (D-051): kill the whole line incl. its newline regardless of column,
    // into the register (accumulating), point at the line start.
    #[test]
    fn emacs_kill_whole_line_kills_the_line_and_newline() {
        let mut st = EditorState::new(b"foo\nbar".to_vec());
        st.set_caret_gravity(CaretGravity::BetweenChar);
        st.set_cursor(1); // mid-line — column is irrelevant
        apply_command(&mut st, &Command::EmacsKillWholeLine);
        assert_eq!(text(&st), "bar");
        assert_eq!(st.view.cursor, 0);
        assert_eq!(st.register().text(), b"foo\n");

        // A second kill-whole-line accumulates (it is a kill): "foo\n" then "bar".
        apply_command(&mut st, &Command::EmacsKillWholeLine);
        assert_eq!(text(&st), "");
        assert_eq!(st.register().text(), b"foo\nbar");
    }

    // Emacs upcase-/downcase-region (C-x C-u / C-x C-l) (D-051): recase the active region, keeping point+mark.
    #[test]
    fn emacs_case_region_recases_the_active_region() {
        let mut up = EditorState::new(b"foo bar".to_vec());
        up.set_caret_gravity(CaretGravity::BetweenChar);
        up.set_cursor(0);
        apply_command(&mut up, &Command::SetMark); // mark at 0
        up.set_cursor(3); // region [0,3) = "foo"
        apply_command(
            &mut up,
            &Command::EmacsCaseRegion {
                case: WordCase::Upcase,
            },
        );
        assert_eq!(text(&up), "FOO bar");
        assert_eq!(
            (up.view.cursor, up.mark()),
            (3, Some(0)),
            "point and mark unchanged"
        );
        assert!(up.register().is_empty());

        // No mark set: inert.
        let mut none = EditorState::new(b"foo".to_vec());
        none.set_caret_gravity(CaretGravity::BetweenChar);
        none.set_cursor(3);
        apply_command(
            &mut none,
            &Command::EmacsCaseRegion {
                case: WordCase::Downcase,
            },
        );
        assert_eq!(text(&none), "foo");
    }

    // Emacs delete-indentation (M-^) (D-051): join this line to the previous, fixing whitespace to one
    // space — or none when the join lands at beginning-of-line (empty previous line). Point at the join.
    #[test]
    fn emacs_delete_indentation_joins_to_previous_line() {
        let join = |buf: &[u8], cur: usize| {
            let mut st = EditorState::new(buf.to_vec());
            st.set_caret_gravity(CaretGravity::BetweenChar);
            st.set_cursor(cur);
            apply_command(&mut st, &Command::EmacsDeleteIndentation);
            (text(&st), st.view.cursor)
        };
        assert_eq!(join(b"foo\n   bar", 7), ("foo bar".into(), 3)); // eats indent -> one space
        assert_eq!(join(b"foo\nbar", 4), ("foo bar".into(), 3)); // no indent -> still one space
        assert_eq!(join(b"foo  \nbar", 6), ("foo bar".into(), 3)); // eats prev trailing ws
        assert_eq!(join(b"\nbar", 1), ("bar".into(), 0)); // empty prev -> no space, join at bol
                                                          // First line: nothing to join to -> inert.
        assert_eq!(join(b"foo", 1), ("foo".into(), 1));
    }

    // The killed text is in the register: a following paste-before restores it.
    #[test]
    fn kill_region_text_round_trips_through_paste() {
        let st = run(
            "hello world\n",
            &[
                Command::SetMark,
                Command::MoveRight,
                Command::MoveRight,
                Command::MoveRight,
                Command::MoveRight,
                Command::MoveRight,
                Command::KillRegion,
                Command::Paste {
                    after: false,
                    count: 1,
                    move_after: false,
                },
            ],
        );
        assert_eq!(text(&st), "hello world\n");
    }

    // M-w copies without deleting; the mark survives (Emacs leaves it set).
    #[test]
    fn copy_region_yanks_without_deleting_and_keeps_the_mark() {
        let st = run(
            "abcdef\n",
            &[
                Command::SetMark,
                Command::MoveRight,
                Command::MoveRight,
                Command::MoveRight,
                Command::CopyRegion,
            ],
        );
        assert_eq!(text(&st), "abcdef\n");
        assert_eq!(st.view.cursor, 3);
        assert_eq!(st.view.mark, Some(0));
    }

    // C-x C-x swaps point and mark, and is involutive.
    #[test]
    fn exchange_point_and_mark_swaps_and_is_involutive() {
        let mut st = EditorState::new(b"abcdef\n".to_vec());
        for c in &[
            Command::SetMark,
            Command::MoveRight,
            Command::MoveRight,
            Command::MoveRight,
        ] {
            apply_command(&mut st, c);
        }
        assert_eq!((st.view.cursor, st.view.mark), (3, Some(0)));
        apply_command(&mut st, &Command::ExchangePointMark);
        assert_eq!((st.view.cursor, st.view.mark), (0, Some(3)));
        apply_command(&mut st, &Command::ExchangePointMark);
        assert_eq!((st.view.cursor, st.view.mark), (3, Some(0)));
    }

    // The region is order-independent: killing works when point is BEFORE the mark too.
    #[test]
    fn kill_region_uses_min_max_when_point_precedes_mark() {
        let st = run(
            "abcdef\n",
            &[
                Command::MoveRight,
                Command::MoveRight,
                Command::MoveRight,
                Command::MoveRight,
                Command::SetMark,
                Command::MoveLeft,
                Command::MoveLeft,
                Command::KillRegion,
            ],
        );
        assert_eq!(text(&st), "abef\n");
    }

    // No mark set: C-w / M-w / C-x C-x are inert (no panic, no change).
    #[test]
    fn region_commands_with_no_mark_are_inert() {
        let st = run(
            "abc\n",
            &[
                Command::MoveRight,
                Command::MoveRight,
                Command::KillRegion,
                Command::CopyRegion,
                Command::ExchangePointMark,
            ],
        );
        assert_eq!(text(&st), "abc\n");
        assert_eq!(st.view.cursor, 2);
        assert_eq!(st.view.mark, None);
    }
}

#[cfg(test)]
mod caret_gravity_tests {
    //! D-050 / RFC-0015: the Emacs profile's caret is BETWEEN characters, so its edits are not Vim-clamped.
    //! These lock the two seams the slice gated (the Normal-mode edit clamp and the charwise-paste cursor)
    //! independent of the parity comparator, and pin that `OnChar` stays byte-identical to Vim.
    use crate::editor::*;

    fn run_with(
        gravity: CaretGravity,
        initial: &str,
        start: usize,
        cmds: &[Command],
    ) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        st.set_caret_gravity(gravity);
        st.set_cursor(start);
        for c in cmds {
            apply_command(&mut st, c);
        }
        st
    }

    #[test]
    fn edit_clamp_is_on_char_only() {
        // `hello world`, delete to end-of-line from col 6 -> `hello `. Vim rests the caret ON the last char
        // (5); Emacs point rests AFTER it (6, the empty end-of-line slot).
        let cmds = [Command::Delete(1, Motion::LineEnd)];
        let vim = run_with(CaretGravity::OnChar, "hello world", 6, &cmds);
        assert_eq!(
            vim.cursor(),
            5,
            "Vim clamps the Normal-mode caret onto the last char"
        );
        let emacs = run_with(CaretGravity::BetweenChar, "hello world", 6, &cmds);
        assert_eq!(
            emacs.cursor(),
            6,
            "Emacs point rests on the after-last slot (not clamped)"
        );
        assert_eq!(String::from_utf8(emacs.bytes().to_vec()).unwrap(), "hello ");
    }

    #[test]
    fn charwise_paste_cursor_follows_gravity() {
        // Isolate the paste-cursor rule from any motion difference: yank `abc`, then paste it inline (both
        // gravities insert at the same offset, so the TEXT is identical) and check only where point lands.
        // Vim rests ON the last pasted byte; the Emacs profile rests AFTER it.
        let cmds = [
            Command::Yank(1, Motion::LineEnd), // register := "abc", caret stays at 0
            Command::Paste {
                after: true,
                count: 1,
                move_after: false,
            }, // insert "abc" after the caret char
        ];
        let vim = run_with(CaretGravity::OnChar, "abc", 0, &cmds);
        assert_eq!(String::from_utf8(vim.bytes().to_vec()).unwrap(), "aabcbc");
        assert_eq!(vim.cursor(), 3, "Vim paste rests on the last pasted byte");
        let emacs = run_with(CaretGravity::BetweenChar, "abc", 0, &cmds);
        assert_eq!(String::from_utf8(emacs.bytes().to_vec()).unwrap(), "aabcbc");
        assert_eq!(
            emacs.cursor(),
            4,
            "Emacs profile rests point after the pasted text"
        );
    }

    #[test]
    fn set_cursor_seeds_curswant_for_vertical_move() {
        // Placing point at col 1 of the last line then moving up must aim at col 1, not col 0 (curswant).
        let st = run_with(
            CaretGravity::BetweenChar,
            "alpha\nbeta\ngamma",
            12,
            &[Command::Move(1, Motion::Up)],
        );
        assert_eq!(
            st.cursor(),
            7,
            "vertical move keeps the placed column (curswant seeded by set_cursor)"
        );
    }

    #[test]
    fn default_gravity_is_on_char() {
        let st = EditorState::new(b"x".to_vec());
        assert_eq!(st.caret_gravity(), CaretGravity::OnChar);
    }
}
