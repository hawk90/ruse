//! The line model: the final-newline slot, grapheme clusters and resource bounds. Split from `editor/tests.rs` (#1397).

/// The empty slot after a final newline is not a line (#677), plus the motions of the v0.1.3 cluster on
/// buffer shapes the Neovim oracle fixtures cannot express (they replay an UNTERMINATED buffer).
#[cfg(test)]
mod final_newline_slot_tests {
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

    fn mv(n: u32, m: Motion) -> Command {
        Command::Move(n, m)
    }

    /// `cmds`, then type `s` in Insert at the cursor and leave it, as `iZ<Esc>` does.
    fn then_insert(mut cmds: Vec<Command>, s: &str) -> Vec<Command> {
        cmds.push(Command::EnterInsert);
        cmds.extend(s.chars().map(Command::InsertChar));
        cmds.push(Command::EnterNormal);
        cmds
    }

    #[test]
    fn linewise_delete_of_the_last_line_leaves_the_cursor_on_a_real_line() {
        // nvim: `Gdd iZ` on `a\nb\n` gives `Za\n`; ruse typed on the phantom slot (`a\nZ`).
        let cmds = vec![mv(1, Motion::LastLine), Command::Delete(1, Motion::Line)];
        assert_eq!(text(&run("a\nb\n", &then_insert(cmds, "Z"))), "Za\n");
        let cmds = vec![mv(1, Motion::Down), Command::Delete(1, Motion::LastLine)];
        assert_eq!(text(&run("a\nb\n", &then_insert(cmds, "Z"))), "Za\n");
        // `Gdip` on `a\n\nb\n` keeps the blank line as the last line.
        let cmds = vec![
            mv(1, Motion::LastLine),
            Command::Delete(1, Motion::InnerParagraph),
        ];
        assert_eq!(text(&run("a\n\nb\n", &then_insert(cmds, "Z"))), "a\nZ\n");
    }

    #[test]
    fn undo_never_rests_on_the_phantom_slot() {
        // nvim: `Adef<Esc>u iZ` on `abc\n` gives `abZc\n`.
        let mut cmds = vec![
            mv(1, Motion::LineEnd),
            Command::EnterInsert,
            Command::MoveRight,
        ];
        cmds.extend("def".chars().map(Command::InsertChar));
        cmds.extend([Command::EnterNormal, Command::Undo]);
        assert_eq!(text(&run("abc\n", &then_insert(cmds, "Z"))), "abZc\n");
    }

    #[test]
    fn motions_running_off_the_end_land_on_the_last_char() {
        // `ww` / `}` / `5g_` / `99go` / `)))` on terminated buffers (nvim).
        assert_eq!(run("foo bar\n", &[mv(2, Motion::WordFwd)]).cursor().0, 6);
        assert_eq!(run("a\nbc\n", &[mv(1, Motion::ParagraphFwd)]).cursor().0, 3);
        assert_eq!(
            run("a  \nb  \n", &[mv(5, Motion::LineLastNonBlank)])
                .cursor()
                .0,
            4
        );
        assert_eq!(run("ab\n", &[mv(99, Motion::GotoByte)]).cursor().0, 1);
        assert_eq!(
            run("A b. C d.\n", &[mv(3, Motion::SentenceFwd)]).cursor().0,
            8
        );
        // An empty last line is a real line: the cursor rests on it.
        assert_eq!(run("a\n\n", &[mv(3, Motion::WordFwd)]).cursor().0, 2);
    }

    #[test]
    fn x_emptying_an_unterminated_last_line_keeps_that_line() {
        // `a\nb` + `jx`: the last line is now empty, spelled like a final newline; the cursor stays on it.
        let st = run("a\nb", &[mv(1, Motion::Down), Command::DeleteUnder(1)]);
        assert_eq!(text(&st), "a\n");
        assert_eq!(st.cursor().0, 2);
    }

    #[test]
    fn operators_never_take_the_final_newline() {
        // `c}`, `jd)`, `ede`, `v}d` on terminated buffers (nvim).
        let cmds = vec![Command::Change(1, Motion::ParagraphFwd)];
        assert_eq!(text(&run("a\nb\n", &then_insert(cmds, "Z"))), "Z\n");
        let st = run(
            "a\nb\n",
            &[mv(1, Motion::Down), Command::Delete(1, Motion::SentenceFwd)],
        );
        assert_eq!(text(&st), "a\n\n");
        let st = run(
            "foo bar \n",
            &[mv(2, Motion::WordEnd), Command::Delete(1, Motion::WordEnd)],
        );
        assert_eq!(text(&st), "foo ba\n");
        let st = run(
            "a\nb\n",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Charwise,
                },
                mv(1, Motion::ParagraphFwd),
                Command::DeleteSelection,
            ],
        );
        assert_eq!(text(&st), "\n");
    }

    #[test]
    fn delete_paragraph_at_eof_is_linewise_but_yank_is_charwise() {
        // nvim `op_delete` promotes `d}` over whole lines; `y}` stays charwise `a\nb`.
        let st = run("a\nb\n", &[Command::Delete(1, Motion::ParagraphFwd)]);
        assert_eq!(text(&st), "");
        assert!(st.register().is_linewise());
        let st = run("a\nb\n", &[Command::Yank(1, Motion::ParagraphFwd)]);
        assert_eq!(st.register().text(), b"a\nb");
        assert!(!st.register().is_linewise());
    }

    #[test]
    fn join_never_turns_the_final_newline_into_a_space() {
        // nvim: `GJ` / `GgJ` leave `a\nb\n`; `5J` gives `a b\n`; `J` on one line is a no-op.
        let st = run("a\nb\n", &[mv(1, Motion::LastLine), Command::JoinLines(2)]);
        assert_eq!(text(&st), "a\nb\n");
        let st = run(
            "a\nb\n",
            &[mv(1, Motion::LastLine), Command::JoinLinesNoSpace(2)],
        );
        assert_eq!(text(&st), "a\nb\n");
        let st = run("a\nb\n", &[Command::JoinLines(5)]);
        assert_eq!(text(&st), "a b\n");
        assert_eq!(st.cursor().0, 1);
        let st = run("foo bar baz qux\n", &[Command::JoinLines(2)]);
        assert_eq!(text(&st), "foo bar baz qux\n");
        let st = run("a\nb\nc\n", &[Command::JoinLines(5)]);
        assert_eq!(text(&st), "a b c\n");
        assert_eq!(st.cursor().0, 3);
    }

    #[test]
    fn ex_ranges_do_not_count_the_phantom_line() {
        let mut st = EditorState::new(b"a\nb\n".to_vec());
        assert_eq!(
            st.delete_lines(SubRange::WholeFile),
            2,
            ":%d reports 2 lines"
        );
        assert_eq!(text(&st), "");
        let mut st = EditorState::new(b"a\nb\n".to_vec());
        assert_eq!(st.join_lines(SubRange::WholeFile, false), 2, ":%j joins 2");
        assert_eq!(text(&st), "a b\n");
        let mut st = EditorState::new(b"a\n".to_vec());
        assert_eq!(
            st.join_lines(SubRange::WholeFile, false),
            0,
            "nothing to join"
        );
        assert_eq!(text(&st), "a\n");
        // `:2d` on `a\nb\n` leaves the cursor on `a`, not the slot after it.
        let mut st = EditorState::new(b"a\nb\n".to_vec());
        st.delete_lines(SubRange::Lines(2, 2));
        assert_eq!(text(&st), "a\n");
        assert_eq!(st.cursor().0, 0);
        let mut st = EditorState::new(b"a\n\nb\n".to_vec());
        let out = st
            .substitute(SubRange::WholeFile, "^$", "x", SubFlags::default())
            .expect("substitute");
        assert_eq!(out.replacements, 1);
        assert_eq!(text(&st), "a\nx\nb\n");
    }

    #[test]
    fn line_delete_through_an_unterminated_last_line_takes_the_newline_above() {
        // #709: `:g/b/d`, `:2,3d`, `jdVj` on unterminated buffers leave no trailing newline (nvim).
        let mut st = EditorState::new(b"a\nb".to_vec());
        st.global(SubRange::WholeFile, "b", false, &GlobalCmd::Delete)
            .expect("global");
        assert_eq!(text(&st), "a");
        let mut st = EditorState::new(b"a\n\nb".to_vec());
        st.global(SubRange::WholeFile, "[ab]", false, &GlobalCmd::Delete)
            .expect("global");
        assert_eq!(text(&st), "", "the kept empty line is the whole buffer");
        let mut st = EditorState::new(b"a\nb\nc".to_vec());
        st.delete_lines(SubRange::Lines(2, 3));
        assert_eq!(text(&st), "a");
        assert_eq!(st.cursor().0, 0);
        let st = run(
            "a\nb\nc",
            &[
                mv(1, Motion::Down),
                Command::OpForced {
                    op: OpKind::Delete,
                    count: 1,
                    motion: Motion::Down,
                    wise: ForcedWise::Linewise,
                },
            ],
        );
        assert_eq!(text(&st), "a");
        assert!(st.register().is_linewise());
        assert_eq!(st.register().text(), b"b\nc\n");
    }

    #[test]
    fn count_dd_on_the_last_line_fails() {
        // Vim: `2dd` on the last line has no line below to reach — a no-op, unlike `5dd` higher up.
        let st = run(
            "a\nb\n",
            &[mv(1, Motion::Down), Command::Delete(2, Motion::Line)],
        );
        assert_eq!(text(&st), "a\nb\n");
        assert_eq!(
            text(&run("a\nb\n", &[Command::Delete(5, Motion::Line)])),
            ""
        );
    }

    #[test]
    fn word_motions_on_terminated_and_multibyte_buffers() {
        // #746 `w`/`b`/`ge` stop on an empty line; #741 `e` advances over multibyte words.
        assert_eq!(run("foo\n\nbar\n", &[mv(1, Motion::WordFwd)]).cursor().0, 4);
        let back = |m| {
            run("foo\n\nbar\n", &[mv(1, Motion::LastLine), mv(1, m)])
                .cursor()
                .0
        };
        assert_eq!(back(Motion::WordBack), 4);
        assert_eq!(back(Motion::BigWordBack), 4);
        assert_eq!(back(Motion::WordEndBack), 4);
        assert_eq!(
            run("가나 다라\n", &[mv(2, Motion::WordEnd)]).cursor().0,
            "가나 다".len()
        );
        // #742: `3dw` crosses the line break; the last word stops at its line end.
        let st = run("foo bar\nbaz qux\n", &[Command::Delete(3, Motion::WordFwd)]);
        assert_eq!(text(&st), "qux\n");
        let st = run(
            "foo bar\nbaz\n",
            &[mv(1, Motion::WordFwd), Command::Delete(3, Motion::WordFwd)],
        );
        assert_eq!(text(&st), "foo \n");
    }

    #[test]
    fn info_and_completion_skip_the_phantom_line() {
        // `g CTRL-G` on the slot reads as the last line, not `Line 2 of 1`.
        let info = crate::info::cursor_pos_info(b"abc\n", BytePos(4), 8, true);
        assert!(info.contains("Line 1 of 1"), "{info}");
        // CTRL-X CTRL-L offers the real empty line but no candidate for the slot after the final newline.
        let (_, cands) = crate::motion::line_completions(b"x\n\ny\n", 0);
        assert_eq!(cands, vec!["y".to_string(), String::new()]);
        let (_, cands) = crate::motion::line_completions(b"x\ny\n", 0);
        assert_eq!(cands, vec!["y".to_string()]);
    }
}

/// #909: a base with combining marks, a flag and a ZWJ sequence are ONE character to `e`/`E`, `de`,
/// `<BS>`, `<Esc>` and the clamp off the phantom last line (nvim v0.12.6, oracle fixtures
/// `e_combining_*`, `de_combining`, `insert_bs_*`, `esc_after_combining_typed`).
#[cfg(test)]
mod cluster_tests {
    use crate::editor::*;

    fn run(initial: &str, cmds: &[Command]) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        for c in cmds {
            apply_command(&mut st, c);
        }
        st
    }

    #[test]
    fn word_end_lands_on_the_base_of_a_composed_char() {
        // nvim: `e` on "éé x" (decomposed) rests on byte 3, the second base; `ee` goes on to `x`.
        let one = run("e\u{301}e\u{301} x", &[Command::Move(1, Motion::WordEnd)]);
        assert_eq!(one.cursor().0, 3);
        let two = run("e\u{301}e\u{301} x", &[Command::Move(2, Motion::WordEnd)]);
        assert_eq!(two.cursor().0, 7);
        let big = run(
            "e\u{301}e\u{301} x",
            &[Command::Move(1, Motion::BigWordEnd)],
        );
        assert_eq!(big.cursor().0, 3);
    }

    #[test]
    fn de_takes_the_last_char_with_its_marks() {
        let st = run("e\u{301}e\u{301} x", &[Command::Delete(1, Motion::WordEnd)]);
        assert_eq!(st.bytes(), b" x");
    }

    #[test]
    fn backspace_deletes_a_whole_cluster() {
        for (init, want) in [("e\u{301}x", "x"), ("a🇰🇷x", "ax"), ("👨\u{200d}👩x", "x")] {
            let st = run(
                init,
                &[
                    Command::Move(1, Motion::LineEnd),
                    Command::EnterInsert,
                    Command::DeleteBack,
                    Command::EnterNormal,
                ],
            );
            assert_eq!(st.as_str(), Some(want), "{init:?}");
        }
    }

    #[test]
    fn esc_steps_back_over_a_whole_cluster() {
        let st = run(
            "x",
            &[
                Command::EnterInsert,
                Command::InsertChar('e'),
                Command::InsertChar('\u{301}'),
                Command::EnterNormal,
            ],
        );
        assert_eq!(st.cursor().0, 0);
    }

    #[test]
    fn the_phantom_line_clamp_lands_on_a_cluster() {
        // `jxj` on "é\n가": `x` empties the last line, the failed `j` clamps off the slot after the final
        // newline onto the last char of the line above — its base, not its combining mark.
        let st = run(
            "e\u{301}\n가",
            &[
                Command::Move(1, Motion::Down),
                Command::DeleteUnder(1),
                Command::Move(1, Motion::Down),
            ],
        );
        assert_eq!(st.cursor().0, 0);
    }

    #[test]
    fn set_cursor_clamps_out_of_range_and_mid_codepoint() {
        // Past the buffer end → clamped to the end (no panic in the following col_of/slice).
        let mut st = EditorState::new(b"abc".to_vec());
        st.set_cursor(BytePos(999));
        assert_eq!(st.cursor().0, 3);

        // Mid-codepoint → snapped back to the char boundary (`é` is 2 bytes).
        let mut st = EditorState::new("é".as_bytes().to_vec());
        st.set_cursor(BytePos(1));
        assert_eq!(st.cursor().0, 0);
    }
}

/// Resource bounds in the core (#682, #683, #684, #733): hostile inputs return at once and stay bounded.
#[cfg(test)]
mod resource_bound_tests {
    use crate::editor::*;

    fn st(text: &str) -> EditorState {
        EditorState::new(text.as_bytes().to_vec())
    }

    #[test]
    fn a_reversed_line_range_is_taken_in_order() {
        // #733: `Lines(3, 1)` from an API caller underflowed; it means lines 1..=3, as `:3,1d` does.
        let mut s = st("a\nb\nc\nd\n");
        assert_eq!(s.delete_lines(SubRange::Lines(3, 1)), 3);
        assert_eq!(s.as_str().unwrap(), "d\n");
        let mut s = st("a\nb\nc\nd\n");
        assert_eq!(s.yank_lines(SubRange::Lines(2, 1)), 2);
        let mut s = st("a\nb\nc\n");
        let out = s
            .substitute(SubRange::Lines(3, 2), "[a-c]", "x", SubFlags::default())
            .unwrap();
        assert_eq!(out.replacements, 2);
        assert_eq!(s.as_str().unwrap(), "a\nx\nx\n");
        let s = st("a\nb\nc\n");
        assert_eq!(
            s.global_marks(SubRange::Lines(3, 1), "[ac]", false)
                .unwrap(),
            vec![0, 2]
        );
    }

    #[test]
    fn number_options_are_clamped_to_nvims_bounds() {
        // #684: an API caller cannot make `>>` allocate a 16 GiB indent.
        let mut s = st("abc\n");
        s.set_option(EditorOption::ShiftWidth(1 << 34));
        s.set_option(EditorOption::TabStop(1 << 34));
        assert_eq!(s.view.indent.shift_width, MAX_OPTION_INT);
        assert_eq!(s.view.indent.tab_width, MAX_TABSTOP);
        s.set_option(EditorOption::TabStop(0));
        assert_eq!(s.view.indent.tab_width, 1);
    }

    #[test]
    fn virtual_replace_with_a_huge_count_is_bounded() {
        // #683: the surplus past the line end is appended at once and capped like a put (64 MiB).
        let mut s = st("ab\ncd\n");
        apply_command(&mut s, &Command::VirtualReplaceChar(999_999_999, 'x'));
        let line = s.bytes().split(|&c| c == b'\n').next().unwrap().len();
        assert_eq!(
            line,
            2 + (1 << 26),
            "the two chars it overwrote, then the capped surplus"
        );
        assert!(s.bytes().ends_with(b"\ncd\n"));
    }

    #[test]
    fn a_raised_interrupt_stops_a_global_and_a_substitute() {
        let mut s = st("a\na\na\n");
        crate::interrupt::raise();
        assert_eq!(
            s.global(SubRange::WholeFile, "a", false, &GlobalCmd::Delete)
                .unwrap(),
            0
        );
        let out = s
            .substitute(SubRange::WholeFile, "a", "b", SubFlags::default())
            .unwrap();
        assert!(crate::interrupt::take());
        assert_eq!(out.replacements, 0);
        assert_eq!(s.as_str().unwrap(), "a\na\na\n");
    }
}
