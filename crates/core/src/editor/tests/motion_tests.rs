//! Motions: words, brackets, line jumps, `f`/`t`, `curswant`, sections and methods. Split from `editor/tests.rs` (#1397).

#[cfg(test)]
mod lazy_wanted_column_tests {
    //! #1231: the wanted column is set pending and measured on first read; the value is the one the eager
    //! measure would have given when it was set.
    use crate::editor::*;

    #[test]
    fn a_pending_column_measures_the_buffer_it_was_set_on() {
        let mut st = EditorState::new(b"\tab\ncd\n".to_vec());
        st.set_cursor(BytePos(2)); // `b`, after a tab
        let want = st.view.curswant.clone();
        let eager = range::wanted_vcol(st.bytes(), 2, st.view.indent.tabs(), true);
        // Testing for MAXCOL does not measure.
        assert!(want != range::MAXCOL);
        assert!(matches!(&want.0, WantKind::At { col, .. } if col.get().is_none()));
        // The buffer changes under it (another View's edit, an Ex command): the pending column still
        // measures the text it was set on.
        st.set_cursor(BytePos(0));
        apply_command(&mut st, &Command::Delete(1, Motion::Right));
        assert_eq!(st.bytes(), b"ab\ncd\n");
        assert_eq!(want.get(), eager);
        assert_ne!(
            eager,
            range::wanted_vcol(st.bytes(), 2, st.view.indent.tabs(), true)
        );
    }

    #[test]
    fn vertical_moves_read_the_pending_column() {
        // `w` sets the column pending; `j` reads it (and keeps it); `k` comes back to the same char.
        let mut st = EditorState::new(b"one two three\nfour five six\n".to_vec());
        apply_command(&mut st, &Command::Move(2, Motion::WordFwd));
        assert_eq!(st.cursor(), BytePos(8));
        apply_command(&mut st, &Command::Move(1, Motion::Down));
        assert_eq!(st.cursor(), BytePos(14 + 8));
        apply_command(&mut st, &Command::Move(1, Motion::Up));
        assert_eq!(st.cursor(), BytePos(8));
        assert_eq!(st.view.curswant(), 8);
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
        assert_eq!(st.cursor().0, 3, "w stops on the '.'");
        let st = run(
            "foo.bar baz",
            &[
                Command::Move(1, Motion::WordFwd),
                Command::Move(1, Motion::WordFwd),
            ],
        );
        assert_eq!(st.cursor().0, 4, "second w → start of 'bar'");
    }

    #[test]
    fn big_word_spans_punctuation() {
        // "foo.bar baz": W → 'baz' (foo.bar is one WORD).
        let st = run("foo.bar baz", &[Command::Move(1, Motion::BigWordFwd)]);
        assert_eq!(st.cursor().0, 8);
    }

    #[test]
    fn word_fwd_at_eol_rests_on_last_char() {
        // Bare `w`/`W` with no next word moves to end-of-line but Normal mode rests ON the last char, not
        // past it. Verified vs nvim v0.12.4 (fixtures w_last_word_rests_on_last_char etc.).
        let st = run("abc def", &[Command::Move(2, Motion::WordFwd)]);
        assert_eq!(
            st.cursor().0,
            6,
            "second w rests on 'f' (last char), not past it"
        );
        let st = run("abc", &[Command::Move(1, Motion::WordFwd)]);
        assert_eq!(st.cursor().0, 2, "w on the only word rests on 'c'");
        let st = run("foo.bar baz", &[Command::Move(2, Motion::BigWordFwd)]);
        assert_eq!(st.cursor().0, 10, "W rests on the last char 'z'");
        // Mid-buffer w still lands on the next word start (clamp is a no-op there).
        let st = run("abc def ghi", &[Command::Move(2, Motion::WordFwd)]);
        assert_eq!(st.cursor().0, 8, "w to a real next word is unaffected");
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
        assert_eq!(st.cursor().0, 3);
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
    fn cw_changes_to_current_word_end_not_next_word() {
        // Vim cw special case: on a non-blank, change up to the END OF THE WORD (no trailing space), and on
        // a word's LAST char change only that char — unlike `ce`, which would jump into the next word.
        // Expects captured from nvim v0.12.4 (oracle fixture cw_on_blank).
        let st = run(
            "foo  bar",
            &[
                Command::Move(2, Motion::Right),
                Command::Change(1, Motion::WordFwd),
            ],
        );
        assert_eq!(
            text(&st),
            "fo  bar",
            "cw on the last 'o' changes only it (then Insert)"
        );
        // Mid-word cw changes to the word end (== ce here).
        let st = run("foo bar", &[Command::Change(1, Motion::WordFwd)]);
        assert_eq!(text(&st), " bar", "cw on 'f' changes 'foo', not 'foo '");
        // On punctuation-adjacent: cw stops at the class boundary.
        let st = run("foo.bar", &[Command::Change(1, Motion::WordFwd)]);
        assert_eq!(text(&st), ".bar", "cw stops at the '.' class boundary");
        // 2cw changes through the second word's end.
        let st = run("a b c", &[Command::Change(2, Motion::WordFwd)]);
        assert_eq!(text(&st), " c", "2cw changes 'a b'");
    }

    #[test]
    fn dw_on_last_word_does_not_join_the_next_line() {
        // Vim: an `w` operator never crosses the newline — `dw` on the last word empties the line instead
        // of joining it. Expects captured from nvim v0.12.4 (oracle fixtures dw_last_word_*).
        // Whole line is one word.
        let st = run("foo\nbar", &[Command::Delete(1, Motion::WordFwd)]);
        assert_eq!(
            text(&st),
            "\nbar",
            "dw deletes 'foo', leaves an empty first line"
        );
        // Trailing whitespace after the last word is deleted too (through EOL, not the newline).
        let st = run("foo   \nbar", &[Command::Delete(1, Motion::WordFwd)]);
        assert_eq!(
            text(&st),
            "\nbar",
            "dw deletes 'foo   ' up to (not incl.) the newline"
        );
        // Mid-line last word with trailing whitespace.
        let st = run(
            "ab cd  \nef",
            &[
                Command::Move(1, Motion::WordFwd),
                Command::Delete(1, Motion::WordFwd),
            ],
        );
        assert_eq!(text(&st), "ab \nef", "wdw deletes 'cd  ' up to EOL");
        // `dW` at EOL likewise stops at the newline.
        let st = run("foo.bar\nbaz", &[Command::Delete(1, Motion::BigWordFwd)]);
        assert_eq!(text(&st), "\nbaz", "dW deletes 'foo.bar', no join");
        // `2dw` where the 2nd word ends the line: both words go, the newline stays.
        let st = run("a b\nc d", &[Command::Delete(2, Motion::WordFwd)]);
        assert_eq!(text(&st), "\nc d", "2dw deletes 'a b' up to EOL, no join");
    }

    #[test]
    fn multibyte_is_one_word() {
        // "가나 다": w skips the Hangul word to the next.
        let st = run("가나 다", &[Command::Move(1, Motion::WordFwd)]);
        assert_eq!(
            st.cursor().0,
            7,
            "w lands on '다' after the multibyte word + space"
        );
    }

    #[test]
    fn whitespace_only_text_is_unchanged_from_word_style() {
        // The pre-existing WORD behavior is preserved for plain words.
        let st = run("foo bar baz", &[Command::Move(2, Motion::WordFwd)]);
        assert_eq!(st.cursor().0, 8);
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
        assert_eq!(st.cursor().0, 4, "from '(' to ')'");
        let st = run(
            "a(bc)d",
            &[Command::Move(4, Motion::Right), pct()], // cursor onto ')'
        );
        assert_eq!(st.cursor().0, 1, "from ')' back to '('");
    }

    #[test]
    fn respects_nesting() {
        // "((x))": outer '(' at 0 ↔ ')' at 4; inner '(' at 1 ↔ ')' at 3.
        let st = run("((x))", &[pct()]);
        assert_eq!(st.cursor().0, 4);
        let st = run("((x))", &[Command::MoveRight, pct()]);
        assert_eq!(st.cursor().0, 3);
    }

    #[test]
    fn finds_first_bracket_forward_when_not_on_one() {
        // cursor at 0 ('a'), first bracket forward is '(' at 2, its match ')' at 5.
        let st = run("ab(cd)", &[pct()]);
        assert_eq!(st.cursor().0, 5);
    }

    #[test]
    fn matches_across_lines() {
        let st = run("(\n)", &[pct()]);
        assert_eq!(st.cursor().0, 2, "% matches across a newline");
    }

    #[test]
    fn matches_by_type_ignoring_other_brackets() {
        // "([)]": '(' at 0 matches ')' at 2, ignoring the '[' — same as Vim.
        let st = run("([)]", &[pct()]);
        assert_eq!(st.cursor().0, 2);
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
        assert_eq!(st.cursor().0, 1, "no closer → no move");
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
    fn gg_keeps_the_wanted_column() {
        // Start on line 3, column 0; gg → line 1 at the wanted column 0, not its first non-blank: nvim's
        // `'nostartofline'` (`beginline(BL_SOL)` keeps `curswant`).
        let st = run(
            "  abc\ndef\nghi",
            &[
                Command::MoveDown,
                Command::MoveDown,
                Command::Move(1, Motion::GotoLine),
            ],
        );
        assert_eq!(st.cursor().0, 0, "gg keeps column 0 on line 1");
    }

    #[test]
    fn cap_g_goes_to_last_line() {
        let st = run("abc\ndef\nxyz", &[Command::Move(1, Motion::LastLine)]);
        assert_eq!(
            st.cursor().0,
            8,
            "G lands on the start of the last line 'xyz'"
        );
    }

    #[test]
    fn count_g_goes_to_that_line() {
        // {2}G → line 2 ('def' starts at byte 4).
        let st = run("abc\ndef\nghi", &[Command::Move(2, Motion::GotoLine)]);
        assert_eq!(st.cursor().0, 4);
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
    fn plus_minus_underscore_are_linewise_operators() {
        // `d+` on line 0 deletes lines 0..=1 (this line + the next), linewise.
        let st = run(
            "one\ntwo\nthree\n",
            &[Command::Delete(1, Motion::DownFirstNonBlank)],
        );
        assert_eq!(text(&st), "three\n", "d+ deletes this line and the next");
        assert!(st.register().is_linewise());

        // `d-` on the last line deletes it and the one above.
        let st = run(
            "one\ntwo\nthree\n",
            &[
                Command::Move(3, Motion::GotoLine),
                Command::Delete(1, Motion::UpFirstNonBlank),
            ],
        );
        assert_eq!(text(&st), "one\n", "d- deletes this line and the one above");

        // `d_` == `dd` (just the cursor's line); `2d_` == `2dd`.
        let st = run(
            "one\ntwo\nthree\n",
            &[Command::Delete(1, Motion::LineUnderscore)],
        );
        assert_eq!(text(&st), "two\nthree\n", "d_ == dd");
        let st = run(
            "one\ntwo\nthree\n",
            &[Command::Delete(2, Motion::LineUnderscore)],
        );
        assert_eq!(text(&st), "three\n", "2d_ == 2dd");
    }

    #[test]
    fn no_phantom_line_after_a_trailing_newline() {
        // `G` on a buffer ending in '\n' lands on the last CONTENT line, not a blank line below it.
        let st = run("one\ntwo\n", &[Command::Move(0, Motion::LastLine)]);
        assert_eq!(
            st.cursor().0,
            4,
            "G lands on 'two', not the phantom line at byte 8"
        );
        // `j` from the last content line does not descend onto the phantom.
        let st = run(
            "one\ntwo\n",
            &[Command::Move(0, Motion::LastLine), Command::MoveDown],
        );
        assert_eq!(
            st.cursor().0,
            4,
            "j on the last line is a no-op (no phantom below)"
        );
        // A genuinely empty last line (before the final '\n') is still reachable.
        let st = run("a\n\n", &[Command::Move(0, Motion::LastLine)]);
        assert_eq!(
            st.cursor().0,
            2,
            "the real empty line at byte 2 is the last line"
        );
        // `dG` still deletes through the last content line (incl. its newline).
        let st = run("one\ntwo\n", &[Command::Delete(1, Motion::LastLine)]);
        assert_eq!(text(&st), "", "dG deletes every line including the last");
    }

    #[test]
    fn count_beyond_end_clamps_to_last_line() {
        let st = run("a\nb\n", &[Command::Move(99, Motion::GotoLine)]);
        // line 99 doesn't exist → clamp to the last CONTENT line ('b' at byte 2). The trailing '\n' is a
        // terminator, not a new empty line (Vim), so the cursor never lands on the phantom slot at byte 4.
        assert_eq!(st.cursor().0, 2);
    }

    #[test]
    fn count_percent_jumps_to_percentage_of_file() {
        // `{count}%` → line (count*line_count+99)/100, first non-blank. Verified vs nvim v0.12.4.
        // 4 lines: 50% → line 2, 25% → line 1, 100% → line 4.
        let st = run("a\nb\nc\nd", &[Command::Move(50, Motion::GotoPercent)]);
        assert_eq!(st.cursor().0, 2, "50% of 4 lines → line 2 ('b')");
        let st = run("a\nb\nc\nd", &[Command::Move(25, Motion::GotoPercent)]);
        assert_eq!(st.cursor().0, 0, "25% → line 1 ('a')");
        let st = run("a\nb\nc\nd", &[Command::Move(100, Motion::GotoPercent)]);
        assert_eq!(st.cursor().0, 6, "100% → line 4 ('d')");
        // `d{count}%` is linewise.
        let st = run("a\nb\nc\nd", &[Command::Delete(50, Motion::GotoPercent)]);
        assert_eq!(text(&st), "c\nd", "d50% deletes lines 1-2 linewise");
    }

    #[test]
    fn goto_byte_moves_to_the_nth_byte() {
        // `{count}go` — count is a 1-based byte offset. "abc\ndef": 'a'=1,'b'=2,'c'=3,'\n'=4,'d'=5...
        let st = run("abc\ndef", &[Command::Move(5, Motion::GotoByte)]);
        assert_eq!(st.cursor().0, 4, "byte 5 (1-based) = offset 4 = 'd'");
        // Bare `go` (count 1) → the first byte.
        let st = run("abc\ndef", &[Command::Move(1, Motion::GotoByte)]);
        assert_eq!(st.cursor().0, 0);
        // A count past the end clamps into range (Normal-mode clamp keeps it off the newline/EOF).
        let st = run("abc", &[Command::Move(99, Motion::GotoByte)]);
        assert_eq!(st.cursor().0, 2, "clamped to the last char");
    }

    #[test]
    fn goto_byte_is_an_exclusive_operator_motion() {
        // `dgo` from byte 5 back to byte 1 deletes the exclusive span [offset 0, offset 4) = "abc\n".
        let st = run(
            "abc\ndef",
            &[
                Command::Move(5, Motion::GotoByte),
                Command::Delete(1, Motion::GotoByte),
            ],
        );
        assert_eq!(text(&st), "def");
        // Forward `d3go` from the start deletes [0, 2) = "ab".
        let st = run("abcdef", &[Command::Delete(3, Motion::GotoByte)]);
        assert_eq!(text(&st), "cdef");
    }

    // Emacs back-to-indentation (M-m) / Vim `^` share Motion::LineFirstNonBlank: land on the first
    // non-blank of the line, or the line end when it is all blank.
    #[test]
    fn line_first_non_blank_lands_on_first_non_blank() {
        let mut st = EditorState::new(b"  foo".to_vec());
        st.set_caret_gravity(CaretGravity::BetweenChar);
        st.set_cursor(BytePos(5)); // end of line
        apply_command(&mut st, &Command::Move(1, Motion::LineFirstNonBlank));
        assert_eq!(
            st.view.cursor, 2,
            "lands on 'f' past the two leading spaces"
        );

        // An all-blank line: land at the line end (no non-blank to find).
        let mut blank = EditorState::new(b"   \nx".to_vec());
        blank.set_caret_gravity(CaretGravity::BetweenChar);
        blank.set_cursor(BytePos(0));
        apply_command(&mut blank, &Command::Move(1, Motion::LineFirstNonBlank));
        assert_eq!(blank.view.cursor, 3, "all-blank line: land at its end");
    }

    // #762: Vim `^` is `beginline(BL_WHITE | BL_FIX)` — on an all-blank line it stops on the LAST blank,
    // under an operator and in Visual too, never on the line break past it. Ground truth: nvim v0.12.6.
    fn vim(initial: &[u8], cmds: &[Command]) -> EditorState {
        let mut st = EditorState::new(initial.to_vec());
        for c in cmds {
            apply_command(&mut st, c);
        }
        st
    }

    #[test]
    fn caret_on_an_all_blank_line_rests_on_the_last_blank() {
        // nvim `^` on ["    ", "x"] -> cursor [1, 3]; on ["\t \t", "x"] -> [1, 2].
        let st = vim(b"    \nx", &[Command::Move(1, Motion::LineFirstNonBlank)]);
        assert_eq!(st.view.cursor, 3);
        let st = vim(b"\t \t\nx", &[Command::Move(1, Motion::LineFirstNonBlank)]);
        assert_eq!(st.view.cursor, 2);
    }

    #[test]
    fn d_caret_on_an_all_blank_line_keeps_the_last_blank() {
        // nvim `0d^` on ["    ", "x"] -> [" ", "x"], register "   " charwise, cursor [1, 0].
        let st = vim(b"    \nx", &[Command::Delete(1, Motion::LineFirstNonBlank)]);
        assert_eq!(text(&st), " \nx");
        assert_eq!(st.register().text(), b"   ");
        assert!(!st.register().is_linewise());
        assert_eq!(st.view.cursor, 0);
        // A count changes nothing (`03d^`), and tabs count as blanks (`0d^` on "\t \t" keeps the last tab).
        let st = vim(b"    \nx", &[Command::Delete(3, Motion::LineFirstNonBlank)]);
        assert_eq!(text(&st), " \nx");
        let st = vim(
            b"\t \t\nx",
            &[Command::Delete(1, Motion::LineFirstNonBlank)],
        );
        assert_eq!(text(&st), "\t\nx");
        assert_eq!(st.register().text(), b"\t ");
    }

    #[test]
    fn c_and_y_caret_on_an_all_blank_line_stop_before_the_last_blank() {
        // nvim `0c^Z<Esc>` -> ["Z ", "x"]; `0y^` yanks "   ".
        let mut st = vim(b"    \nx", &[Command::Change(1, Motion::LineFirstNonBlank)]);
        assert_eq!(text(&st), " \nx");
        assert_eq!(st.register().text(), b"   ");
        assert_eq!(st.mode(), Mode::Insert);
        apply_command(&mut st, &Command::InsertChar('Z'));
        assert_eq!(text(&st), "Z \nx");
        let st = vim(b"    \nx", &[Command::Yank(1, Motion::LineFirstNonBlank)]);
        assert_eq!(text(&st), "    \nx");
        assert_eq!(st.register().text(), b"   ");
    }

    #[test]
    fn visual_caret_on_an_all_blank_line_does_not_select_the_line_break() {
        // nvim `0v^d` on ["    ", "x"] -> ["", "x"], register "    " charwise: the lines stay apart.
        let st = vim(
            b"    \nx",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Charwise,
                },
                Command::Move(1, Motion::LineFirstNonBlank),
            ],
        );
        assert_eq!(st.view.cursor, 3);
        let mut st = st;
        apply_command(&mut st, &Command::DeleteSelection);
        assert_eq!(text(&st), "\nx");
        assert_eq!(st.register().text(), b"    ");
        assert!(!st.register().is_linewise());
    }

    #[test]
    fn emacs_back_to_indentation_keeps_the_line_end_on_an_all_blank_line() {
        // Emacs 31.1 `back-to-indentation` on "   " -> point 3 (the oracle's back_to_indentation_all_blank).
        let mut st = EditorState::new(b"   \nx".to_vec());
        st.set_caret_gravity(CaretGravity::BetweenChar);
        st.set_cursor(BytePos(0));
        apply_command(&mut st, &Command::Move(1, Motion::LineFirstNonBlank));
        assert_eq!(st.view.cursor, 3);
    }
}

#[cfg(test)]
mod find_char_tests {
    use crate::editor::*;

    fn find(ch: char, forward: bool, till: bool) -> Motion {
        Motion::FindChar {
            ch,
            forward,
            till,
            repeat: false,
        }
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
        assert_eq!(st.cursor().0, 3, "fx lands on x");
        let st = run("abcxef", &[Command::Move(1, find('x', true, true))]);
        assert_eq!(st.cursor().0, 2, "tx stops one before x");
    }

    #[test]
    fn count_finds_the_nth() {
        let st = run("axbxcx", &[Command::Move(2, find('x', true, false))]);
        assert_eq!(st.cursor().0, 3, "2fx lands on the second x");
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
        assert_eq!(st.cursor().0, 2, "Fx searches backward onto x");
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
        assert_eq!(st.cursor().0, 0, "f does not cross the newline");
    }

    #[test]
    fn multibyte_target() {
        // "a가b" — find the multibyte '가' (bytes 1..4).
        let st = run("a가b", &[Command::Move(1, find('가', true, false))]);
        assert_eq!(st.cursor().0, 1, "lands on the multibyte char boundary");
    }

    #[test]
    fn missing_target_is_a_noop() {
        let st = run("abc", &[Command::Move(1, find('z', true, false))]);
        assert_eq!(st.cursor().0, 0);
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
        motion::col_of(b, motion::line_start(b, st.cursor().0), st.cursor().0)
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
        assert_eq!(st.cursor().0, 8); // the 'n' in "lmnop"
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
        assert_eq!(st.cursor().0, 3); // the 'd' in "abcd"
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
        assert_eq!(st.cursor().0, 6); // the 'z' in "wxyz" (not past it)
        let st2 = run(
            "ab\nwxyz\nc\n",
            &[
                Command::Move(1, Motion::LineEnd),
                Command::Move(2, Motion::Down), // ride down two lines to short "c"
            ],
        );
        assert_eq!(st2.cursor().0, 8); // the 'c' (its only char)
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
        assert_eq!(st.cursor().0, 10); // the 'n' in "lmnop" (col 2), proving curswant = 2 not MAXCOL
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
        assert_eq!(st.cursor().0, 2); // on 'X' after the Esc nudge
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
        assert_eq!(st.cursor().0, 4); // on 'X' (col 4 of "betaX")
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
        assert_eq!(st.cursor().0, 0);
    }
}

#[cfg(test)]
mod section_motion_tests {
    //! Vim section motions `]]` / `[[` / `][` / `[]` — forward/backward to a `{`/`}` (or form-feed) in
    //! column 0. Every `assert` here is pinned to nvim v0.12.4 (see the `section_*` oracle fixtures in
    //! tests/parity/vim/fixtures/corpus.yaml).
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

    // Buffer with two brace sections: line starts 0,2,6,10,12,16,18.
    // "{\n  a\n  b\n}\n{\n  c\n}"  (lines: {, "  a", "  b", }, {, "  c", })
    const B: &str = "{\n  a\n  b\n}\n{\n  c\n}";

    #[test]
    fn forward_section_start_lands_on_next_brace() {
        // `]]` from line 1 (`{`) skips itself → the next `{` in column 0 (line 5, byte 12).
        let st = run(B, &[Command::Move(1, Motion::SectionFwd)]);
        assert_eq!(st.cursor().0, 12);
        // `2]]` runs out of sections after line 5 → clamps to the last content line (`}`, byte 18).
        let st = run(B, &[Command::Move(2, Motion::SectionFwd)]);
        assert_eq!(st.cursor().0, 18);
    }

    #[test]
    fn forward_section_end_lands_on_next_close_brace() {
        // `][` from line 1 → the next `}` in column 0 (line 4, byte 10).
        let st = run(B, &[Command::Move(1, Motion::SectionEndFwd)]);
        assert_eq!(st.cursor().0, 10);
    }

    #[test]
    fn backward_section_motions_skip_the_current_line() {
        // From the last line (`}`, byte 18): `[[` → previous `{` (line 5, byte 12); `[]` → previous `}`
        // (line 4, byte 10) — strictly before the cursor, so it does not stop on the `}` under the cursor.
        let st = run(B, &[Command::Move(1, Motion::LastLine)]);
        assert_eq!(st.cursor().0, 18, "G lands on the last close brace");
        let st = run(
            B,
            &[
                Command::Move(1, Motion::LastLine),
                Command::Move(1, Motion::SectionBack),
            ],
        );
        assert_eq!(st.cursor().0, 12);
        let st = run(
            B,
            &[
                Command::Move(1, Motion::LastLine),
                Command::Move(1, Motion::SectionEndBack),
            ],
        );
        assert_eq!(st.cursor().0, 10);
    }

    #[test]
    fn eof_and_bof_clamp_to_first_non_blank() {
        // `]]` with no further `{` clamps to the last line's first non-blank ("    xy" → byte at 'x').
        let st = run("{\n  a\n    xy", &[Command::Move(1, Motion::SectionFwd)]);
        assert_eq!(st.cursor().0, 10, "'    xy' first non-blank");
        // `[[`/`[]` with nothing before clamp to the first line (byte 0 here).
        let st = run(
            "{\n  a\n    xy",
            &[
                Command::Move(1, Motion::LastLine),
                Command::Move(1, Motion::SectionEndBack),
            ],
        );
        assert_eq!(st.cursor().0, 0);
    }

    #[test]
    fn form_feed_is_a_boundary_for_both_directions() {
        // A form-feed (\x0C) in column 0 is a section boundary for `]]` AND `][`.
        let st = run("aaa\n\x0C\nbbb", &[Command::Move(1, Motion::SectionFwd)]);
        assert_eq!(st.cursor().0, 4, "]] stops at the form-feed line");
        let st = run("aaa\n\x0C\nbbb", &[Command::Move(1, Motion::SectionEndFwd)]);
        assert_eq!(st.cursor().0, 4, "][ stops at the form-feed line");
    }

    #[test]
    fn indented_brace_is_not_a_boundary() {
        // Only a brace in COLUMN 0 counts; an indented ` {` is skipped.
        // Line starts: "a"@0, " {"@2, "b"@5, "{"@7, "c"@9.
        let st = run("a\n {\nb\n{\nc", &[Command::Move(1, Motion::SectionFwd)]);
        assert_eq!(
            st.cursor().0,
            7,
            "skips the indented brace, stops at the col-0 brace (byte 7)"
        );
    }

    #[test]
    fn d_forward_section_is_linewise_and_excludes_the_target_line() {
        // `d]]` from line 1 deletes whole lines 1-4 (linewise), leaving the second section.
        let st = run(B, &[Command::Delete(1, Motion::SectionFwd)]);
        assert_eq!(text(&st), "{\n  c\n}");
        assert!(st.register().is_linewise());
        assert_eq!(st.register().text(), b"{\n  a\n  b\n}\n");
        assert_eq!(st.cursor().0, 0);
        // `d][` from line 1 deletes lines 1-3 (up to but not including the `}` at line 4).
        let st = run(B, &[Command::Delete(1, Motion::SectionEndFwd)]);
        assert_eq!(text(&st), "}\n{\n  c\n}");
        assert!(st.register().is_linewise());
        assert_eq!(st.register().text(), b"{\n  a\n  b\n");
    }

    #[test]
    fn y_forward_section_yanks_linewise_without_moving_text() {
        let st = run(B, &[Command::Yank(1, Motion::SectionFwd)]);
        assert_eq!(text(&st), B, "yank leaves the buffer unchanged");
        assert!(st.register().is_linewise());
        assert_eq!(st.register().text(), b"{\n  a\n  b\n}\n");
        assert_eq!(st.cursor().0, 0);
    }

    #[test]
    fn d_backward_section_deletes_from_target_up_to_the_cursor_line() {
        // Buffer "x\n{\na\n}\n{\nb\n}" line starts 0,2,4,6,8,10,12.
        // From line 5 (`{`, byte 8), `d[[` deletes lines 2-4 (previous `{` at line 2 through the line above).
        let d = "x\n{\na\n}\n{\nb\n}";
        let st = run(
            d,
            &[
                Command::Move(5, Motion::GotoLine),
                Command::Delete(1, Motion::SectionBack),
            ],
        );
        assert_eq!(text(&st), "x\n{\nb\n}");
        assert!(st.register().is_linewise());
        assert_eq!(st.register().text(), b"{\na\n}\n");
        // `d[]` from line 6 (`b`, byte 10) deletes lines 4-5 (previous `}` at line 4 through line 5).
        let st = run(
            d,
            &[
                Command::Move(6, Motion::GotoLine),
                Command::Delete(1, Motion::SectionEndBack),
            ],
        );
        assert_eq!(text(&st), "x\n{\na\nb\n}");
        assert!(st.register().is_linewise());
        assert_eq!(st.register().text(), b"}\n{\n");
    }

    #[test]
    fn backward_section_at_bof_is_a_noop_operator() {
        // `d[[` on the first line with no previous `{` moves nothing → nothing is deleted (Vim fails the op).
        let st = run("x\n{\na\n}", &[Command::Delete(1, Motion::SectionBack)]);
        assert_eq!(text(&st), "x\n{\na\n}");
        assert!(st.register().is_empty());
    }
}

#[cfg(test)]
mod unmatched_bracket_motion_tests {
    //! Vim `[(` / `])` / `[{` / `]}` — jump to the enclosing UNMATCHED paren/brace. Every `assert` is pinned
    //! to nvim v0.12.4 (see the `unmatched_*` oracle fixtures in tests/parity/vim/fixtures/corpus.yaml).
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

    // "(abcdef)": ( a b c d e f )  = bytes 0 1 2 3 4 5 6 7. Cursor placed on 'c' (byte 3) via 3 x MoveRight.
    fn on_c(brackets: &str) -> Vec<Command> {
        let _ = brackets;
        vec![Command::MoveRight; 3]
    }

    #[test]
    fn bare_moves_reach_the_enclosing_bracket() {
        // `[(` goes to the enclosing open paren; `])` to the enclosing close paren.
        let mut cmds = on_c("(abcdef)");
        cmds.push(Command::Move(1, Motion::UnmatchedParenBack));
        assert_eq!(run("(abcdef)", &cmds).cursor().0, 0);

        let mut cmds = on_c("(abcdef)");
        cmds.push(Command::Move(1, Motion::UnmatchedParenFwd));
        assert_eq!(run("(abcdef)", &cmds).cursor().0, 7);

        // Braces behave identically.
        let mut cmds = on_c("{abcdef}");
        cmds.push(Command::Move(1, Motion::UnmatchedBraceBack));
        assert_eq!(run("{abcdef}", &cmds).cursor().0, 0);
        let mut cmds = on_c("{abcdef}");
        cmds.push(Command::Move(1, Motion::UnmatchedBraceFwd));
        assert_eq!(run("{abcdef}", &cmds).cursor().0, 7);
    }

    #[test]
    fn nesting_is_respected_and_count_steps_out_levels() {
        // "(a(b)c)": ( a ( b ) c )  = 0 1 2 3 4 5 6. Cursor on 'b' (byte 3).
        let one = run(
            "(a(b)c)",
            &[
                Command::MoveRight,
                Command::MoveRight,
                Command::MoveRight,
                Command::Move(1, Motion::UnmatchedParenBack),
            ],
        );
        assert_eq!(one.cursor().0, 2, "[( → the INNER open paren");
        let two = run(
            "(a(b)c)",
            &[
                Command::MoveRight,
                Command::MoveRight,
                Command::MoveRight,
                Command::Move(2, Motion::UnmatchedParenBack),
            ],
        );
        assert_eq!(two.cursor().0, 0, "2[( → out two levels to the OUTER open");

        // Forward mirror on braces: "{a{b}c}" cursor on 'b' (byte 3).
        let one = run(
            "{a{b}c}",
            &[
                Command::MoveRight,
                Command::MoveRight,
                Command::MoveRight,
                Command::Move(1, Motion::UnmatchedBraceFwd),
            ],
        );
        assert_eq!(one.cursor().0, 4, "]}} → the inner close brace");
        let two = run(
            "{a{b}c}",
            &[
                Command::MoveRight,
                Command::MoveRight,
                Command::MoveRight,
                Command::Move(2, Motion::UnmatchedBraceFwd),
            ],
        );
        assert_eq!(
            two.cursor().0,
            6,
            "2]}} → out two levels to the outer close"
        );
    }

    #[test]
    fn no_enclosing_bracket_is_a_noop_move() {
        // No paren at all → the cursor does not move.
        let st = run(
            "abcdef",
            &[
                Command::MoveRight,
                Command::Move(1, Motion::UnmatchedParenBack),
            ],
        );
        assert_eq!(st.cursor().0, 1);
        // Cursor ON the open paren: `[(` scans strictly left, finds nothing → no-op (nvim: stays put).
        let st = run("(abc)", &[Command::Move(1, Motion::UnmatchedParenBack)]);
        assert_eq!(st.cursor().0, 0);
        // Cursor ON the close paren: `])` scans strictly right → no-op.
        let st = run(
            "(abc)",
            &[
                Command::Move(1, Motion::LineEnd),
                Command::Move(1, Motion::UnmatchedParenFwd),
            ],
        );
        assert_eq!(st.cursor().0, 4);
    }

    #[test]
    fn operator_is_exclusive_charwise() {
        // `d[(` from 'c' (byte 3) deletes [open, cursor) = "(ab" (the cursor char 'c' and the close paren stay).
        let mut cmds = on_c("(abcdef)");
        cmds.push(Command::Delete(1, Motion::UnmatchedParenBack));
        let st = run("(abcdef)", &cmds);
        assert_eq!(text(&st), "cdef)");
        assert!(!st.register().is_linewise());
        assert_eq!(st.register().text(), b"(ab");
        assert_eq!(st.cursor().0, 0);

        // `d])` from 'c' deletes [cursor, close) = "cdef" — the `)` is EXCLUDED (verified: not `d%`'s inclusive).
        let mut cmds = on_c("(abcdef)");
        cmds.push(Command::Delete(1, Motion::UnmatchedParenFwd));
        let st = run("(abcdef)", &cmds);
        assert_eq!(text(&st), "(ab)");
        assert!(!st.register().is_linewise());
        assert_eq!(st.register().text(), b"cdef");
        assert_eq!(st.cursor().0, 3);
    }

    #[test]
    fn yank_and_change_use_the_same_span() {
        // `y])` captures "cdef" charwise without moving text; cursor rests at the span start.
        let mut cmds = on_c("(abcdef)");
        cmds.push(Command::Yank(1, Motion::UnmatchedParenFwd));
        let st = run("(abcdef)", &cmds);
        assert_eq!(text(&st), "(abcdef)");
        assert_eq!(st.register().text(), b"cdef");
        assert_eq!(st.cursor().0, 3);

        // `c]}` on braces deletes the inner span and enters insert; typing replaces it.
        let st = run(
            "{abcdef}",
            &[
                Command::MoveRight,
                Command::MoveRight,
                Command::MoveRight,
                Command::Change(1, Motion::UnmatchedBraceFwd),
                Command::InsertChar('X'),
                Command::EnterNormal,
            ],
        );
        assert_eq!(text(&st), "{abX}");
    }

    #[test]
    fn operator_off_column_zero_target_becomes_linewise() {
        // "foo(\nbar\n)baz": the `)` starts line 3 (column 0). `d])` from 'b' of "bar" is exclusive charwise
        // ending at column 0, so Vim's exclusive-linewise rule deletes the whole "bar" line (nvim-verified).
        let st = run(
            "foo(\nbar\n)baz",
            &[
                Command::Move(2, Motion::GotoLine), // to line 2, first non-blank ('b')
                Command::Delete(1, Motion::UnmatchedParenFwd),
            ],
        );
        assert_eq!(text(&st), "foo(\n)baz");
        assert!(st.register().is_linewise());
        assert_eq!(st.register().text(), b"bar\n");
    }

    #[test]
    fn operator_with_no_bracket_deletes_nothing() {
        let st = run(
            "abcdef",
            &[
                Command::MoveRight,
                Command::Delete(1, Motion::UnmatchedParenFwd),
            ],
        );
        assert_eq!(text(&st), "abcdef");
        assert!(st.register().is_empty());
    }
}

#[cfg(test)]
mod method_motion_tests {
    //! Vim `]m`/`[m`/`]M`/`[M` under operators. Every expected buffer/register is pinned to nvim v0.12.4.
    //! The motions are exclusive charwise and take the shared exclusive-linewise reduction, so a target on a
    //! column-0 brace deletes whole lines.
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
    // Line 3's first non-blank is `x` (byte 35), inside method a's body.
    const CLASS: &str =
        "class Foo {\n    void a() {\n        x;\n    }\n    void b() {\n        y;\n    }\n}\n";
    const FLAT: &str = "void a() {\n    x;\n}\nvoid b() {\n    y;\n}\n";

    #[test]
    fn charwise_mid_line_landings() {
        // From `x` (byte 35): `d]m` deletes to method b's `{` (byte 57) — exclusive charwise, mid-line.
        let st = run(
            CLASS,
            &[
                Command::Move(3, Motion::GotoLine),
                Command::Move(1, Motion::LineFirstNonBlank),
                Command::Delete(1, Motion::MethodStartFwd),
            ],
        );
        assert_eq!(
            text(&st),
            "class Foo {\n    void a() {\n        {\n        y;\n    }\n}\n"
        );
        assert!(!st.register().is_linewise());
        assert_eq!(st.register().text(), b"x;\n    }\n    void b() ");

        // `d[M` deletes back to the class `{` (byte 10) — charwise.
        let st = run(
            CLASS,
            &[
                Command::Move(3, Motion::GotoLine),
                Command::Move(1, Motion::LineFirstNonBlank),
                Command::Delete(1, Motion::MethodEndBack),
            ],
        );
        assert_eq!(
            text(&st),
            "class Foo x;\n    }\n    void b() {\n        y;\n    }\n}\n"
        );
        assert_eq!(st.register().text(), b"{\n    void a() {\n        ");
    }

    #[test]
    fn column_zero_landing_becomes_linewise() {
        // `d4]m` from the top lands on the class's closing `}` at column 0 → whole-line delete (nvim: `V`).
        let st = run(CLASS, &[Command::Delete(4, Motion::MethodStartFwd)]);
        assert_eq!(text(&st), "}\n");
        assert!(st.register().is_linewise());

        // FLAT `d2]m` lands on the first block's top-level `}` (column 0) → linewise over lines 1-2.
        let st = run(FLAT, &[Command::Delete(2, Motion::MethodStartFwd)]);
        assert_eq!(text(&st), "}\nvoid b() {\n    y;\n}\n");
        assert!(st.register().is_linewise());
        assert_eq!(st.register().text(), b"void a() {\n    x;\n");
    }

    #[test]
    fn no_brace_operator_deletes_nothing() {
        let st = run("abcdef\n", &[Command::Delete(1, Motion::MethodStartFwd)]);
        assert_eq!(text(&st), "abcdef\n");
        assert!(st.register().is_empty());
    }
}
