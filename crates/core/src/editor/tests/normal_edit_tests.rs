//! Normal-mode edits: single keys, shifts, `gq` formatting and operator spans. Split from `editor/tests.rs` (#1397).

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
        assert_eq!(st.cursor().0, 1, "r leaves the cursor on the replaced char");
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
        assert_eq!(st.cursor().0, 4);
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
        assert_eq!(st.cursor().0, 2, "cursor lands on the last replaced char");
        // Fewer than `count` chars remain on the line: a clean no-op (Vim never partial-replaces).
        let st = run("ab", &[Command::ReplaceChar(3, 'z')]);
        assert_eq!(text(&st), "ab");
        assert_eq!(st.cursor().0, 0);
    }

    #[test]
    fn replace_char_with_newline_splits_the_line() {
        // `r<CR>` replaces the char with a line break (Vim splits the line), cursor on the new line's start.
        // Verified vs nvim v0.12.4 (fixture replace_char_with_newline).
        let st = run(
            "abcdef",
            &[
                Command::Move(2, Motion::Right),
                Command::ReplaceChar(1, '\n'),
            ],
        );
        assert_eq!(
            text(&st),
            "ab\ndef",
            "replacing 'c' with a newline splits after 'ab'"
        );
        assert_eq!(st.cursor().0, 3, "cursor on 'd' (start of the new line)");
        // `{count}r<CR>` replaces count chars with a SINGLE newline, not count newlines.
        let st = run("abcdef", &[Command::ReplaceChar(3, '\n')]);
        assert_eq!(
            text(&st),
            "\ndef",
            "3r<CR> removes 'abc', inserts one line break"
        );
        assert_eq!(st.cursor().0, 1);
    }

    #[test]
    fn delete_under_with_count() {
        // `3x` deletes three chars into the unnamed register (charwise); clamps at EOL.
        let st = run("abcdef", &[Command::DeleteUnder(3)]);
        assert_eq!(text(&st), "def");
        assert_eq!(st.cursor().0, 0);
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
        assert_eq!(st.cursor().0, 1);
        // On a non-letter, `~` just moves right.
        let st = run("1a", &[Command::ToggleCase(1)]);
        assert_eq!(text(&st), "1a");
        assert_eq!(st.cursor().0, 1);
    }

    #[test]
    fn toggle_case_with_count() {
        // `3~` toggles three chars, leaving the cursor past the last (clamped at EOL).
        let st = run("abcdef", &[Command::ToggleCase(3)]);
        assert_eq!(text(&st), "ABCdef");
        assert_eq!(st.cursor().0, 3);
        // Clamp: fewer than `count` chars left toggles to EOL. The cursor would land past the last char,
        // but Vim never rests the Normal-mode cursor on the newline, so `commit` pulls it onto the last char.
        let st = run("aB", &[Command::ToggleCase(9)]);
        assert_eq!(text(&st), "Ab");
        assert_eq!(st.cursor().0, 1);
    }

    #[test]
    fn join_lines_uses_one_space_and_drops_indent() {
        let st = run("foo\n   bar", &[Command::JoinLines(1)]);
        assert_eq!(text(&st), "foo bar");
        assert_eq!(st.cursor().0, 3, "cursor lands on the joined space");
    }

    #[test]
    fn join_on_last_line_is_noop() {
        let st = run("only", &[Command::JoinLines(1)]);
        assert_eq!(text(&st), "only");
    }

    #[test]
    fn count_join_joins_count_lines() {
        // `{count}J` joins count lines (count-1 seams), cursor on the last join. Expects from nvim v0.12.4.
        let st = run("a\nb\nc\nd", &[Command::JoinLines(3)]);
        assert_eq!(text(&st), "a b c\nd", "3J joins three lines");
        assert_eq!(
            st.cursor().0,
            3,
            "cursor on the last join (the space before 'c')"
        );
        // `J`/`2J` both do a single join.
        let st = run("a\nb\nc", &[Command::JoinLines(2)]);
        assert_eq!(text(&st), "a b\nc");
        // A count past the end joins what it can, then stops.
        let st = run("a\nb", &[Command::JoinLines(9)]);
        assert_eq!(text(&st), "a b");
        // `{count}gJ` joins without spaces, keeping leading whitespace.
        let st = run("a\n  b\n  c", &[Command::JoinLinesNoSpace(3)]);
        assert_eq!(text(&st), "a  b  c", "3gJ removes only the newlines");
    }

    #[test]
    fn join_suppresses_space_before_close_paren() {
        // Vim inserts no space when the next line's first non-blank is ')'.
        let st = run("foo(\n   )", &[Command::JoinLines(1)]);
        assert_eq!(text(&st), "foo()");
    }

    #[test]
    fn join_does_not_double_a_trailing_space() {
        // The current line already ends in whitespace → no extra space is added.
        let st = run("foo \n   bar", &[Command::JoinLines(1)]);
        assert_eq!(text(&st), "foo bar");
    }

    #[test]
    fn join_empty_line_adds_no_leading_space() {
        // Joining an empty line onto the next inserts no space.
        let st = run("\n   bar", &[Command::JoinLines(1)]);
        assert_eq!(text(&st), "bar");
    }

    #[test]
    fn join_no_space_keeps_indent_and_inserts_nothing() {
        // gJ removes only the newline: the next line's leading whitespace is preserved, no space added.
        let st = run("foo\n   bar", &[Command::JoinLinesNoSpace(1)]);
        assert_eq!(text(&st), "foo   bar");
        assert_eq!(st.cursor().0, 3, "cursor rests at the join seam");
    }

    #[test]
    fn join_no_space_on_last_line_is_noop() {
        let st = run("only", &[Command::JoinLinesNoSpace(1)]);
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
        assert_eq!(st.cursor().0, 4, "cursor on the last digit of the result");
        // Cursor BEFORE the number still finds it forward on the line.
        let st = run("x = 4", &[Command::IncrementNumber(3)]);
        assert_eq!(text(&st), "x = 7");
        // A carry grows the digit count; cursor on the new last digit.
        let st = run("9", &[Command::IncrementNumber(1)]);
        assert_eq!(text(&st), "10");
        assert_eq!(st.cursor().0, 1);
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
    fn increment_preserves_leading_zero_width() {
        // Zero-padded fields keep their width (Vim): 007 -> 008.
        let st = run("007", &[Command::IncrementNumber(1)]);
        assert_eq!(text(&st), "008");
        // Width grows only on carry: 099 -> 100.
        let st = run("099", &[Command::IncrementNumber(1)]);
        assert_eq!(text(&st), "100");
        // Negative padded field keeps the magnitude width: -007 -> -006.
        let st = run("-007", &[Command::IncrementNumber(1)]);
        assert_eq!(text(&st), "-006");
        // Decrementing into more digits pads: 008 - 9 -> -001.
        let st = run("008", &[Command::IncrementNumber(-9)]);
        assert_eq!(text(&st), "-001");
        // No leading zero => no padding: 42 -> 43.
        let st = run("42", &[Command::IncrementNumber(1)]);
        assert_eq!(text(&st), "43");
        // A bare single zero is not a padded field: 0 -> 1 (not 01).
        let st = run("0", &[Command::IncrementNumber(1)]);
        assert_eq!(text(&st), "1");
    }

    #[test]
    fn increment_hex_literal_stays_hex() {
        // 0x1f + 1 → 0x20 (prefix preserved); a carry widens: 0xff + 1 → 0x100.
        let st = run("0x1f", &[Command::IncrementNumber(1)]);
        assert_eq!(text(&st), "0x20");
        let st = run("0xff", &[Command::IncrementNumber(1)]);
        assert_eq!(text(&st), "0x100");
        // Cursor INSIDE the hex digits (on the '1') still adjusts the whole literal.
        let st = run(
            "0x1f",
            &[Command::Move(2, Motion::Right), Command::IncrementNumber(1)],
        );
        assert_eq!(text(&st), "0x20");
        // A plain decimal next to no 0x is unaffected by the hex path.
        let st = run("value 10 end", &[Command::IncrementNumber(1)]);
        assert_eq!(text(&st), "value 11 end");
    }

    /// #717: the bytes nvim v0.12.6 writes (`addsub_*` parity fixtures; nvim `do_addsub`).
    fn incremented(line: &str, right: u32, delta: i64) -> (String, usize) {
        let mut cmds = Vec::new();
        if right > 0 {
            cmds.push(Command::Move(right, Motion::Right));
        }
        cmds.push(Command::IncrementNumber(delta));
        let st = run(line, &cmds);
        (text(&st), st.cursor().0)
    }

    #[test]
    fn increment_decimal_saturates_and_wraps_as_a_u64_magnitude() {
        let d45 = "123456789012345678901234567890123456789012345";
        // Too many digits: read as u64::MAX and left at it (not `1`).
        assert_eq!(incremented(d45, 0, 1), ("18446744073709551615".into(), 19));
        assert_eq!(incremented(d45, 0, -1).0, "18446744073709551615");
        assert_eq!(
            incremented(&format!("-{d45}"), 0, 1).0,
            "-18446744073709551615"
        );
        // Wrapping flips the sign, as nvim's unsigned arithmetic does.
        assert_eq!(
            incremented("18446744073709551615", 0, 1).0,
            "-18446744073709551615"
        );
        assert_eq!(
            incremented("-18446744073709551615", 0, -1).0,
            "18446744073709551615"
        );
        assert_eq!(
            incremented("9223372036854775807", 0, 1).0,
            "9223372036854775808"
        );
        assert_eq!(incremented("-3", 0, 5), ("2".into(), 0));
        assert_eq!(incremented("-01", 0, 1), ("00".into(), 1));
        assert_eq!(incremented("000", 0, -1), ("-001".into(), 3));
    }

    #[test]
    fn increment_hex_keeps_case_and_width_and_wraps() {
        // Hex digits take the case of the number's last letter, `x`/`X` included.
        assert_eq!(incremented("0xAB", 0, 1), ("0xAC".into(), 3));
        assert_eq!(incremented("0X19", 0, 1).0, "0X1A");
        assert_eq!(incremented("0x9F", 0, 1).0, "0xA0");
        assert_eq!(incremented("0xaB", 0, 1).0, "0xAC");
        assert_eq!(incremented("0xAb", 0, 1).0, "0xac");
        assert_eq!(incremented("0Xab", 0, 1).0, "0Xac");
        // Leading zeros keep the width.
        assert_eq!(incremented("0x00ff", 0, 1), ("0x0100".into(), 5));
        assert_eq!(incremented("0x0100", 0, -1).0, "0x00ff");
        // Unsigned, modulo 2^64; a too-long literal saturates and keeps its width.
        assert_eq!(incremented("0x0", 0, -1), ("0xffffffffffffffff".into(), 17));
        assert_eq!(
            incremented("0xffffffffffffffff", 0, 1).0,
            "0x0000000000000000"
        );
        assert_eq!(
            incremented("0xffffffffffffffffffff", 0, 1).0,
            "0x0000ffffffffffffffff"
        );
        // The cursor on the digits (`$`) or the `x` finds the literal.
        assert_eq!(incremented("0xff", 3, 1), ("0x100".into(), 4));
        assert_eq!(incremented("a 0x1f", 3, 1).0, "a 0x20");
        // A `-` before a based literal is not a sign.
        assert_eq!(incremented("-0x10", 0, 1), ("-0x11".into(), 4));
        assert_eq!(incremented("-0x10", 0, -1).0, "-0x0f");
        // `0x` with no hex digit after it is the decimal `0`.
        assert_eq!(incremented("0xg", 0, 1), ("1xg".into(), 0));
    }

    #[test]
    fn increment_binary_keeps_width_and_octal_is_not_a_default_format() {
        assert_eq!(incremented("0b101", 0, 1).0, "0b110");
        assert_eq!(incremented("0b111", 0, 1).0, "0b1000");
        assert_eq!(incremented("0b0011", 0, 1), ("0b0100".into(), 5));
        assert_eq!(incremented("0b10", 0, -2).0, "0b00");
        assert_eq!(incremented("0B1", 0, 1).0, "0B10");
        assert_eq!(incremented("0b101", 4, 1).0, "0b110");
        assert_eq!(incremented("-0b11", 0, 1).0, "-0b100");
        let (wrapped, cursor) = incremented("0b0", 0, -1);
        assert_eq!((wrapped, cursor), (format!("0b{}", "1".repeat(64)), 65));
        // Default `nrformats=bin,hex`: `0o17` and `017` are decimals.
        assert_eq!(incremented("0o17", 0, 1), ("1o17".into(), 0));
        assert_eq!(incremented("0o17", 3, 1).0, "0o18");
        assert_eq!(incremented("017", 0, 1), ("018".into(), 2));
    }

    /// Visual increment over `line`: `right` moves before `v`, `extend` moves after it.
    fn visual_incremented(line: &str, right: u32, extend: Option<Motion>, delta: i64) -> String {
        let mut cmds = Vec::new();
        if right > 0 {
            cmds.push(Command::Move(right, Motion::Right));
        }
        cmds.push(Command::EnterVisual {
            kind: SelectKind::Charwise,
        });
        if let Some(m) = extend {
            cmds.push(Command::Move(1, m));
        }
        cmds.push(Command::IncrementSelection {
            delta,
            sequential: false,
        });
        text(&run(line, &cmds))
    }

    #[test]
    fn visual_increment_reads_only_the_selection() {
        // No walk back to the digit run's start or to a `0x`, and no read past the selection.
        assert_eq!(
            visual_incremented("0x19", 1, Some(Motion::LineEnd), 1),
            "0x20"
        );
        assert_eq!(
            visual_incremented("199", 1, Some(Motion::LineEnd), 1),
            "1100"
        );
        assert_eq!(
            visual_incremented("12345", 1, Some(Motion::Right), 1),
            "12445"
        );
        // A `-` is a sign only when it is selected.
        assert_eq!(visual_incremented("-5", 1, None, 1), "-6");
        assert_eq!(visual_incremented("-5", 0, Some(Motion::LineEnd), 1), "-4");
        assert_eq!(visual_incremented("-1", 0, Some(Motion::LineEnd), 1), "0");
        assert_eq!(visual_incremented("0", 0, None, -1), "-1");
        assert_eq!(
            visual_incremented("-0x10", 0, Some(Motion::LineEnd), 1),
            "-0x11"
        );
        assert_eq!(
            visual_incremented("0xAB", 0, Some(Motion::LineEnd), 1),
            "0xAC"
        );
    }

    #[test]
    fn visual_increment_bumps_every_selected_line() {
        // Linewise-select all three lines, `CTRL-X`-style +1 to the first number on each.
        let st = run(
            "1\n5\n9",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Linewise,
                },
                Command::Move(2, Motion::Down),
                Command::IncrementSelection {
                    delta: 1,
                    sequential: false,
                },
            ],
        );
        assert_eq!(text(&st), "2\n6\n10");
        assert_eq!(st.mode(), Mode::Normal, "returns to Normal");
        assert_eq!(st.cursor().0, 0, "caret on the first changed line");
        // Lines without a number are skipped, not errored; the first number on a line is the target.
        let st = run(
            "a 3 b\nnope\nx 10",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Linewise,
                },
                Command::Move(2, Motion::Down),
                Command::IncrementSelection {
                    delta: -1,
                    sequential: false,
                },
            ],
        );
        assert_eq!(text(&st), "a 2 b\nnope\nx 9");
    }

    #[test]
    fn visual_sequential_increment_makes_a_run() {
        // `g CTRL-A` over a column of 1s → 1,2,3 (adds delta, 2·delta, 3·delta to successive numbered lines).
        let st = run(
            "1\n1\n1",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Linewise,
                },
                Command::Move(2, Motion::Down),
                Command::IncrementSelection {
                    delta: 1,
                    sequential: true,
                },
            ],
        );
        assert_eq!(text(&st), "2\n3\n4");
        // A blank (numberless) line does NOT advance the sequence multiplier.
        let st = run(
            "0\n\n0\n0",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Linewise,
                },
                Command::Move(3, Motion::Down),
                Command::IncrementSelection {
                    delta: 10,
                    sequential: true,
                },
            ],
        );
        assert_eq!(text(&st), "10\n\n20\n30");
    }

    #[test]
    fn visual_increment_caret_lands_on_first_selected_line_left_edge() {
        // Vim leaves the caret on the FIRST SELECTED line at the selection's left-edge column, NOT on the
        // number's last digit and NOT on the first line that actually changed. Verified vs nvim v0.12.4.
        //
        // Multi-digit result: `007`→`008` — caret at col 0 (line start), not the last digit (col 2).
        let st = run(
            "007\n008",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Linewise,
                },
                Command::Move(1, Motion::Down),
                Command::IncrementSelection {
                    delta: 1,
                    sequential: false,
                },
            ],
        );
        assert_eq!(text(&st), "008\n009");
        assert_eq!(
            st.cursor().0,
            0,
            "caret at col 0 of the first line, not on the last digit"
        );
        // Negative result width: `-3`→`-2` — caret at col 0, not on the digit after the sign.
        let st = run(
            "-3\n-3",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Linewise,
                },
                Command::Move(1, Motion::Down),
                Command::IncrementSelection {
                    delta: 1,
                    sequential: true,
                },
            ],
        );
        assert_eq!(text(&st), "-2\n-1");
        assert_eq!(
            st.cursor().0,
            0,
            "caret at col 0, not on the digit after the '-'"
        );
        // First selected line has NO number: the caret still homes there (col 0), not on the changed line.
        let st = run(
            "abc\n5\n5",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Linewise,
                },
                Command::Move(2, Motion::Down),
                Command::IncrementSelection {
                    delta: 1,
                    sequential: true,
                },
            ],
        );
        assert_eq!(text(&st), "abc\n6\n7");
        assert_eq!(
            st.cursor().0,
            0,
            "caret on the first SELECTED line (numberless), not the first CHANGED"
        );
        // Charwise: caret at the selection's start column on the first line (here col 2, onto the number).
        let st = run(
            "a 07",
            &[
                Command::Move(2, Motion::Right), // selection starts at col 2 (the '0')
                Command::EnterVisual {
                    kind: SelectKind::Charwise,
                },
                Command::Move(1, Motion::Right),
                Command::IncrementSelection {
                    delta: 1,
                    sequential: false,
                },
            ],
        );
        assert_eq!(text(&st), "a 08");
        assert_eq!(
            st.cursor().0,
            2,
            "caret at the charwise selection's left edge"
        );
    }

    #[test]
    fn visual_increment_without_a_number_is_a_noop() {
        let st = run(
            "abc\ndef",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Linewise,
                },
                Command::Move(1, Motion::Down),
                Command::IncrementSelection {
                    delta: 1,
                    sequential: false,
                },
            ],
        );
        assert_eq!(text(&st), "abc\ndef");
        assert_eq!(st.mode(), Mode::Normal);
    }

    #[test]
    fn blockwise_increment_targets_the_block_column_not_the_first_number() {
        // The footgun case: each line has a number BEFORE the block. A blockwise CTRL-A over the SECOND
        // number's column must increment that column (5→6, 6→7), leaving the leading 1/2 untouched.
        let st = run(
            "foo 1 bar 5\nfoo 2 bar 6",
            &[
                Command::Move(10, Motion::Right), // onto the '5' (column 10)
                Command::EnterVisual {
                    kind: SelectKind::Blockwise,
                },
                Command::Move(1, Motion::Down), // block spans the '5'/'6' column, rows 0..1
                Command::IncrementSelection {
                    delta: 1,
                    sequential: false,
                },
            ],
        );
        assert_eq!(text(&st), "foo 1 bar 6\nfoo 2 bar 7");
        assert_eq!(st.mode(), Mode::Normal);
        // A block on the middle digit reads only the selected digits, as nvim does (#717,
        // `addsub_block_clip`): `5`→`6`, `9`→`10`.
        let st = run(
            "15\n29",
            &[
                Command::Move(1, Motion::Right), // onto the '5' / '9' (column 1)
                Command::EnterVisual {
                    kind: SelectKind::Blockwise,
                },
                Command::Move(1, Motion::Down),
                Command::IncrementSelection {
                    delta: 1,
                    sequential: false,
                },
            ],
        );
        assert_eq!(text(&st), "16\n210");
    }

    #[test]
    fn charwise_increment_targets_the_selection_not_the_first_number() {
        // `v` selecting only the '5' (with a '1' earlier on the line) increments the 5, not the 1.
        let st = run(
            "1 and 5",
            &[
                Command::Move(6, Motion::Right), // onto the '5'
                Command::EnterVisual {
                    kind: SelectKind::Charwise,
                },
                Command::IncrementSelection {
                    delta: 1,
                    sequential: false,
                },
            ],
        );
        assert_eq!(text(&st), "1 and 6");
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
        assert_eq!(st.cursor().0, 4, "`. returns to the last change position");
    }

    #[test]
    fn goto_last_change_before_any_edit_is_noop() {
        let st = run("abc", &[Command::GotoLastChange]);
        assert_eq!(st.cursor().0, 0, "no last change yet → cursor unmoved");
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
        let change2 = st.cursor().0; // where we are is near change #2's line; capture positions via nav
                                     // g; → newest change (#2), g; again → older (#1 at byte 0), g, → back to newest (#2).
        let mut st = st;
        crate::editor::apply_command(&mut st, &Command::GotoOlderChange);
        let newest = st.cursor().0;
        crate::editor::apply_command(&mut st, &Command::GotoOlderChange);
        assert_eq!(
            st.cursor().0,
            0,
            "second g; reaches the oldest change at byte 0"
        );
        crate::editor::apply_command(&mut st, &Command::GotoNewerChange);
        assert_eq!(st.cursor().0, newest, "g, returns to the newer change");
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
        assert_eq!(st.cursor().0, 3, "`a returns to where mark a was set");
    }

    #[test]
    fn goto_unset_named_mark_is_noop() {
        let st = run("abc", &[Command::GotoNamedMark('q')]);
        assert_eq!(st.cursor().0, 0, "jumping an unset mark does not move");
    }

    #[test]
    fn operator_to_mark() {
        use crate::command::MarkOp;
        // `` d`a ``: exclusive charwise from cursor to mark. Set mark a at 0, move to byte 4, delete back
        // to a → removes [0,4). Verified vs nvim v0.12.4 (fixture d_backtick_mark_charwise).
        let st = run(
            "abc def ghi",
            &[
                Command::SetNamedMark('a'),
                Command::Move(4, Motion::Right),
                Command::OpToMark {
                    op: MarkOp::Delete,
                    name: 'a',
                    linewise: false,
                },
            ],
        );
        assert_eq!(text(&st), "def ghi", "d`a deletes cursor..mark charwise");
        // `d'a`: linewise over the line range. Mark on line 1, cursor on line 3 → delete lines 1..=3.
        let st = run(
            "one\ntwo\nthree\nfour",
            &[
                Command::SetNamedMark('a'),
                Command::Move(3, Motion::GotoLine),
                Command::OpToMark {
                    op: MarkOp::Delete,
                    name: 'a',
                    linewise: true,
                },
            ],
        );
        assert_eq!(text(&st), "four", "d'a deletes whole lines mark..cursor");
        // Unset mark → no-op.
        let st = run(
            "abc",
            &[Command::OpToMark {
                op: MarkOp::Delete,
                name: 'z',
                linewise: false,
            }],
        );
        assert_eq!(text(&st), "abc");
    }

    #[test]
    fn case_to_mark() {
        use crate::command::{MarkOp, WordCase};
        // `` gU`a ``: charwise upcase [cursor, mark). Mark at byte 4, cursor to byte 8 → upcase [4,8).
        // Verified vs nvim v0.12.4 (fixture gU_backtick_mark_charwise).
        let st = run(
            "abc def ghi",
            &[
                Command::Move(4, Motion::Right),
                Command::SetNamedMark('a'),
                Command::Move(4, Motion::Right),
                Command::OpToMark {
                    op: MarkOp::Case(WordCase::Upcase),
                    name: 'a',
                    linewise: false,
                },
            ],
        );
        assert_eq!(
            text(&st),
            "abc DEF ghi",
            "gU`a upcases cursor..mark charwise"
        );
        assert_eq!(st.cursor().0, 4, "cursor lands at the span start");
        // Direction independence: mark AFTER the cursor recases the same span.
        let st = run(
            "ABC DEF GHI",
            &[
                Command::Move(8, Motion::Right),
                Command::SetNamedMark('a'),
                Command::Move(4, Motion::Left),
                Command::OpToMark {
                    op: MarkOp::Case(WordCase::Downcase),
                    name: 'a',
                    linewise: false,
                },
            ],
        );
        assert_eq!(
            text(&st),
            "ABC def GHI",
            "gu`a lowercases min..max regardless of order"
        );
        // `` gU'a ``: LINEWISE upcase over the line range (mark line .. cursor line).
        let st = run(
            "one two\nthree four\nfive six",
            &[
                Command::SetNamedMark('a'),
                Command::Move(3, Motion::GotoLine),
                Command::OpToMark {
                    op: MarkOp::Case(WordCase::Upcase),
                    name: 'a',
                    linewise: true,
                },
            ],
        );
        assert_eq!(
            text(&st),
            "ONE TWO\nTHREE FOUR\nFIVE SIX",
            "gU'a upcases whole lines"
        );
    }

    #[test]
    fn shift_to_mark() {
        use crate::command::MarkOp;
        // `` >'a ``: linewise shift over lines mark..cursor (default shiftwidth 4, spaces).
        let st = run(
            "a\nb\nc",
            &[
                Command::SetNamedMark('a'),
                Command::Move(3, Motion::GotoLine),
                Command::OpToMark {
                    op: MarkOp::Shift { left: false },
                    name: 'a',
                    linewise: true,
                },
            ],
        );
        assert_eq!(
            text(&st),
            "    a\n    b\n    c",
            ">'a shifts all three lines (linewise)"
        );
        // `` >`a ``: the CHARWISE (backtick) form. Cursor rests at col 0 of the far line, so the
        // exclusive-motion rule drops that line — only lines 1-2 shift. Verified vs nvim (fixture
        // shift_right_backtick_mark).
        let st = run(
            "a\nb\nc",
            &[
                Command::SetNamedMark('a'),
                Command::Move(3, Motion::GotoLine),
                Command::OpToMark {
                    op: MarkOp::Shift { left: false },
                    name: 'a',
                    linewise: false,
                },
            ],
        );
        assert_eq!(
            text(&st),
            "    a\n    b\nc",
            ">`a excludes the col-0 far line (exclusive motion)"
        );
        // `` <`a ``: charwise dedent. Mark at line 1 col 0, cursor at line 3 col 0 — the exclusive span
        // `[line1col0, line3col0)` drops line 3 (its only in-range column is 0, excluded), so lines 1-2
        // dedent. Verified vs nvim (fixture shift_left_backtick_mark).
        let st = run(
            "    a\n    b\n    c",
            &[
                Command::SetNamedMark('a'),
                Command::Move(3, Motion::GotoLine),
                Command::MoveLineStart,
                Command::OpToMark {
                    op: MarkOp::Shift { left: true },
                    name: 'a',
                    linewise: false,
                },
            ],
        );
        assert_eq!(
            text(&st),
            "a\nb\n    c",
            "<`a dedents lines 1-2 (far col-0 line excluded)"
        );
    }

    #[test]
    fn reindent_to_mark() {
        use crate::command::MarkOp;
        // `` =`a ``: reindent the spanned lines by the C rules, as nvim's `=` with no 'indentexpr' (#1163;
        // the same buffer from nvim gives these columns). Mark on the opener line, cursor on the closer
        // line; the statements get one indent level.
        let st = run(
            "{\na;\nb;\n}",
            &[
                Command::SetNamedMark('a'),
                Command::Move(4, Motion::GotoLine),
                Command::OpToMark {
                    op: MarkOp::Reindent,
                    name: 'a',
                    linewise: true,
                },
            ],
        );
        assert_eq!(
            text(&st),
            "{\n    a;\n    b;\n}",
            "=`a reindents by the C rules"
        );
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
            st.cursor().0,
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
            st.cursor().0,
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
        assert_eq!(st.cursor().0, 5, "gi resumes at the last-insert caret");
        assert_eq!(text(&st), "ab\nXYcd", "no text change from gi itself");
    }

    #[test]
    fn gi_before_any_insert_goes_to_start() {
        let st = run("abc", &[Command::InsertAtLastInsert]);
        assert_eq!(st.mode(), Mode::Insert);
        assert_eq!(st.cursor().0, 0, "no prior insert → start of buffer");
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
            st.cursor().0 <= text(&st).len(),
            "mark jump stays in range after shrink"
        );
    }

    /// `$ma0llD`: mark `a` on the `f` of "abcdef", then the line cut to "ab" — the mark now sits past it.
    fn shortened_under_mark_a() -> Vec<Command> {
        vec![
            Command::Move(1, Motion::LineEnd),
            Command::SetNamedMark('a'),
            Command::Move(1, Motion::LineStart),
            Command::Move(2, Motion::Right),
            Command::Delete(1, Motion::LineEnd),
        ]
    }

    fn op_to_a(op: crate::command::MarkOp) -> Command {
        Command::OpToMark {
            op,
            name: 'a',
            linewise: false,
        }
    }

    #[test]
    fn op_to_a_mark_past_a_shortened_line_takes_its_last_char() {
        use crate::command::MarkOp;
        // #728, nvim v0.12.6 (fixtures i728_d_bt_after_D, i728_y_bt_after_D): from the next line's start,
        // `` d`a `` / `` y`a `` reach back to the `b` the mark clamps onto, and the exclusive end at column 0
        // pulls back off the newline — so only "b" goes.
        let mut cmds = shortened_under_mark_a();
        cmds.extend([
            Command::Move(1, Motion::Down),
            Command::Move(1, Motion::LineStart),
            op_to_a(MarkOp::Delete),
        ]);
        let st = run("abcdef\nxyz", &cmds);
        assert_eq!(text(&st), "a\nxyz");
        assert_eq!(st.cursor().0, 0);
        let mut cmds = shortened_under_mark_a();
        cmds.extend([
            Command::Move(1, Motion::Down),
            Command::Move(1, Motion::LineStart),
            op_to_a(MarkOp::Yank),
        ]);
        let st = run("abcdef\nxyz", &cmds);
        assert_eq!(text(&st), "ab\nxyz");
        assert_eq!(st.cursor().0, 1, "y`a rests on the clamped mark");
        // Forward on the same line (i728_d_bt_after_D_fwd): `0d`a` takes "a" only, not the whole "ab".
        let mut cmds = shortened_under_mark_a();
        cmds.extend([Command::Move(1, Motion::LineStart), op_to_a(MarkOp::Delete)]);
        let st = run("abcdef\nxyz", &cmds);
        assert_eq!(text(&st), "b\nxyz");
    }

    #[test]
    fn visual_jump_to_a_mark_past_a_shortened_line_rests_on_its_newline() {
        // #728, nvim v0.12.6 (i728_v_bt_after_D): Visual lets the cursor sit one past the last char, so
        // `` v`a `` from the next line stops on "ab"'s newline (byte 2) rather than on `b`.
        let mut cmds = shortened_under_mark_a();
        cmds.extend([
            Command::Move(1, Motion::Down),
            Command::Move(1, Motion::LineStart),
            Command::EnterVisual {
                kind: SelectKind::Charwise,
            },
            Command::GotoNamedMark('a'),
        ]);
        let st = run("abcdef\nxyz", &cmds);
        assert_eq!(st.cursor().0, 2);
    }

    #[test]
    fn quote_mark_onto_a_line_left_all_blank_takes_its_last_blank() {
        // #728, nvim v0.12.6 (i728_qt_after_D): `$ma0wD` leaves "  "; `'a` lands on its last blank
        // (`beginline(BL_WHITE | BL_FIX)`), not the newline after it.
        let st = run(
            "  abc def",
            &[
                Command::Move(1, Motion::LineEnd),
                Command::SetNamedMark('a'),
                Command::Move(1, Motion::LineStart),
                Command::Move(1, Motion::WordFwd),
                Command::Delete(1, Motion::LineEnd),
                Command::GotoNamedMarkLine('a'),
            ],
        );
        assert_eq!(text(&st), "  ");
        assert_eq!(st.cursor().0, 1);
    }

    #[test]
    fn older_change_on_a_shortened_line_rests_on_its_last_char() {
        // #728, nvim v0.12.6 (i728_gsemi_after_D): `$x` records a change at column 6, `0lD` cuts the line to
        // "a"; from the next line `g;` lands on that `a`, not on its newline.
        let st = run(
            "abc def\nx",
            &[
                Command::Move(1, Motion::LineEnd),
                Command::DeleteForward(1),
                Command::Move(1, Motion::LineStart),
                Command::Move(1, Motion::Right),
                Command::Delete(1, Motion::LineEnd),
                Command::Move(1, Motion::Down),
                Command::GotoOlderChange,
            ],
        );
        assert_eq!(text(&st), "a\nx");
        assert_eq!(st.cursor().0, 0);
    }

    #[test]
    fn jumplist_records_jumps_and_ctrl_o_ctrl_i_walk_it() {
        // Lines: "one\ntwo\nthree\nfour" — jump gg (to line 1) then G (to last line), building the jumplist,
        // then CTRL-O walks back and CTRL-I forward.
        let src = "one\ntwo\nthree\nfour";
        // Start on line 2, jump to line 1 (gg), then to last line (G).
        let mut st = crate::editor::EditorState::new(src.as_bytes().to_vec());
        crate::editor::apply_command(&mut st, &Command::Move(2, Motion::Down)); // → line 3 area (not a jump)
        let before_gg = st.cursor().0;
        crate::editor::apply_command(&mut st, &Command::Move(1, Motion::GotoLine)); // gg → line 1 (a jump)
        assert_eq!(st.cursor().0, 0, "gg to line 1");
        crate::editor::apply_command(&mut st, &Command::Move(0, Motion::LastLine)); // G → last line (a jump)
        let at_last = st.cursor().0;
        // CTRL-O: first back saves the current (last line), steps to the newest recorded jump (line-1 pos 0).
        crate::editor::apply_command(&mut st, &Command::GotoOlderJump);
        assert_eq!(st.cursor().0, 0, "CTRL-O returns to the gg position");
        // CTRL-O again: to the position gg jumped FROM (before_gg).
        crate::editor::apply_command(&mut st, &Command::GotoOlderJump);
        assert_eq!(
            st.cursor().0,
            before_gg,
            "second CTRL-O to where gg started"
        );
        // CTRL-I forward returns toward the newer jumps.
        crate::editor::apply_command(&mut st, &Command::GotoNewerJump);
        assert_eq!(st.cursor().0, 0, "CTRL-I forward to the gg position");
        crate::editor::apply_command(&mut st, &Command::GotoNewerJump);
        assert_eq!(
            st.cursor().0,
            at_last,
            "CTRL-I forward to the saved last-line position"
        );
    }

    #[test]
    fn jumplist_nav_is_noop_without_jumps() {
        let st = run("abc", &[Command::GotoOlderJump, Command::GotoNewerJump]);
        assert_eq!(st.cursor().0, 0, "nothing recorded → no movement");
    }

    #[test]
    fn context_mark_jumps_back_and_toggles() {
        let src = "one\ntwo\nthree\nfour";
        let mut st = crate::editor::EditorState::new(src.as_bytes().to_vec());
        crate::editor::apply_command(&mut st, &Command::Move(2, Motion::Down)); // → line 3 (not a jump)
        let origin = st.cursor().0;
        crate::editor::apply_command(&mut st, &Command::Move(1, Motion::GotoLine)); // gg (a jump) → 0
        assert_eq!(st.cursor().0, 0);
        // `` `` `` jumps back to where gg left from (the context mark).
        crate::editor::apply_command(&mut st, &Command::GotoContextMark);
        assert_eq!(st.cursor().0, origin, "`` returns to the pre-jump position");
        // Repeating toggles back to where we just were (0), because `` is itself a jump.
        crate::editor::apply_command(&mut st, &Command::GotoContextMark);
        assert_eq!(
            st.cursor().0,
            0,
            "repeated `` toggles to the other position"
        );
    }

    #[test]
    fn context_mark_is_noop_without_jumps() {
        let st = run(
            "abc",
            &[Command::GotoContextMark, Command::GotoContextMarkLine],
        );
        assert_eq!(st.cursor().0, 0, "no jump recorded → no movement");
    }

    #[test]
    fn plain_motions_do_not_record_jumps() {
        // h/j/k/l/w are NOT jumps, so CTRL-O after them does nothing.
        let st = run(
            "hello world",
            &[Command::Move(3, Motion::Right), Command::GotoOlderJump],
        );
        assert_eq!(
            st.cursor().0,
            3,
            "a plain motion is not a jump; CTRL-O is a no-op"
        );
    }

    #[test]
    fn change_list_nav_is_noop_at_the_ends_and_before_edits() {
        // No edits → g;/g, do nothing.
        let st = run("abc", &[Command::GotoOlderChange, Command::GotoNewerChange]);
        assert_eq!(st.cursor().0, 0, "no changes → nav is a no-op");
        // One edit, then g, (newer) with nothing newer is a no-op; g; reaches it.
        let mut st = run("abcd", &[Command::DeleteUnder(1)]);
        let at_edit = st.cursor().0;
        crate::editor::apply_command(&mut st, &Command::GotoNewerChange);
        assert_eq!(st.cursor().0, at_edit, "g, at the newest end does not move");
        crate::editor::apply_command(&mut st, &Command::GotoOlderChange);
        assert_eq!(st.cursor().0, at_edit, "g; lands on the single change");
    }

    // --- Vim `[`/`]` change/yank marks (`` `[ `` / `` `] `` / `'[` / `']`) --------------------------

    #[test]
    fn yank_sets_change_marks_around_the_word() {
        // "foo bar baz": onto "bar" (byte 4), yiw, then jump the marks.
        let start = run(
            "foo bar baz",
            &[
                Command::Move(4, Motion::Right),
                Command::Yank(1, Motion::InnerWord),
                Command::Move(3, Motion::Right), // move away first
                Command::GotoChangeMarkStart,
            ],
        )
        .cursor()
        .0;
        assert_eq!(start, 4, "`[ lands on the first char of the yanked word");
        let end = run(
            "foo bar baz",
            &[
                Command::Move(4, Motion::Right),
                Command::Yank(1, Motion::InnerWord),
                Command::GotoChangeMarkEnd,
            ],
        )
        .cursor()
        .0;
        assert_eq!(end, 6, "`] lands on the last char of the yanked word");
    }

    #[test]
    fn insert_sets_change_marks_around_the_typed_run() {
        // `ihello<Esc>` over "abc" → "helloabc"; `[ = the 'h' (byte 0), `] = the insert end-caret (byte 5,
        // one past the last inserted char — Neovim's convention), which is interior here so it is not clamped.
        let typed = [
            Command::EnterInsert,
            Command::InsertChar('h'),
            Command::InsertChar('e'),
            Command::InsertChar('l'),
            Command::InsertChar('l'),
            Command::InsertChar('o'),
            Command::EnterNormal,
        ];
        let start = {
            let mut cmds = typed.to_vec();
            cmds.push(Command::GotoChangeMarkStart);
            run("abc", &cmds).cursor().0
        };
        assert_eq!(start, 0, "`[ is the first inserted char");
        let end = {
            let mut cmds = typed.to_vec();
            cmds.push(Command::GotoChangeMarkEnd);
            run("abc", &cmds).cursor().0
        };
        assert_eq!(
            end, 5,
            "`] is the insert end-caret (one past the last inserted char)"
        );
    }

    #[test]
    fn delete_collapses_change_marks_to_the_deletion_point() {
        // Delete 'c' from "abcdef" → "abdef"; both marks collapse onto the deletion point (byte 2).
        let start = run(
            "abcdef",
            &[
                Command::Move(2, Motion::Right),
                Command::DeleteUnder(1),
                Command::Move(2, Motion::Right), // move away
                Command::GotoChangeMarkStart,
            ],
        )
        .cursor()
        .0;
        assert_eq!(start, 2, "`[ is the deletion point");
        let end = run(
            "abcdef",
            &[
                Command::Move(2, Motion::Right),
                Command::DeleteUnder(1),
                Command::Move(2, Motion::Right),
                Command::GotoChangeMarkEnd,
            ],
        )
        .cursor()
        .0;
        assert_eq!(
            end, 2,
            "`] is the deletion point too (a pure delete collapses the marks)"
        );
    }

    #[test]
    fn linewise_put_sets_change_marks_around_the_pasted_line() {
        // `yyp` over "one\ntwo" → "one\none\ntwo"; the marks bracket the PUT line (2nd "one", bytes 4..7).
        let base = [
            Command::Yank(1, Motion::Line),
            Command::Paste {
                after: true,
                count: 1,
                move_after: false,
            },
        ];
        let start = {
            let mut cmds = base.to_vec();
            cmds.push(Command::GotoChangeMarkStart);
            run("one\ntwo", &cmds).cursor().0
        };
        assert_eq!(start, 4, "`[ is the first char of the put line");
        let end = {
            let mut cmds = base.to_vec();
            cmds.push(Command::GotoChangeMarkEnd);
            run("one\ntwo", &cmds).cursor().0
        };
        assert_eq!(
            end, 6,
            "`] is the last char of the put line (trailing '\\n' skipped by the EOL clamp)"
        );
    }

    #[test]
    fn linewise_change_marks_land_on_first_non_blank_of_first_and_last_line() {
        // `2yy` over "  aa\n  bb\ncc" yanks lines 1-2; `'[` / `']` go to the first non-blank of each.
        let start = run(
            "  aa\n  bb\ncc",
            &[
                Command::Yank(2, Motion::Line),
                Command::Move(2, Motion::Down), // move away to line 3
                Command::GotoChangeMarkStartLine,
            ],
        )
        .cursor()
        .0;
        assert_eq!(
            start, 2,
            "'[ is the first non-blank of the first yanked line"
        );
        let end = run(
            "  aa\n  bb\ncc",
            &[
                Command::Yank(2, Motion::Line),
                Command::GotoChangeMarkEndLine,
            ],
        )
        .cursor()
        .0;
        assert_eq!(end, 7, "'] is the first non-blank of the last yanked line");
    }

    #[test]
    fn change_marks_are_noop_before_any_change() {
        let st = run(
            "abc",
            &[
                Command::Move(1, Motion::Right),
                Command::GotoChangeMarkStart,
            ],
        );
        assert_eq!(st.cursor().0, 1, "no change/yank yet → `[ does not move");
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
    fn shift_right_adds_tab_width_spaces_and_keeps_the_wanted_column() {
        // Default config = 4 spaces. Matches the parity fixture shift_right_line (oracle `>>`): under
        // 'nostartofline' the cursor stays at the wanted column, here 0 (#1176).
        let st = run("hello", &[Command::ShiftRight(1)]);
        assert_eq!(text(&st), "    hello");
        assert_eq!(
            st.cursor().0,
            0,
            "cursor stays at column 0, on the new indent"
        );
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
        assert_eq!(st.cursor().0, 0);
    }

    #[test]
    fn shift_right_uses_a_tab_when_indent_style_is_tab() {
        let st = run_indent("hello", 4, IndentStyle::Tab, &[Command::ShiftRight(1)]);
        assert_eq!(text(&st), "\thello");
        assert_eq!(st.cursor().0, 0, "cursor on the tab that covers column 0");
    }

    #[test]
    fn insert_tab_expandtab_fills_to_next_tabstop() {
        // At column 0 a full tabstop of spaces is inserted.
        let st = run_indent(
            "hello",
            4,
            IndentStyle::Space,
            &[Command::EnterInsert, Command::InsertTab],
        );
        assert_eq!(text(&st), "    hello");
        assert_eq!(st.cursor().0, 4);

        // Mid-line (caret after "he", vcol 2): only 2 spaces to reach the next tabstop (col 4).
        let st = run_indent(
            "hello",
            4,
            IndentStyle::Space,
            &[
                Command::Move(2, Motion::Right),
                Command::EnterInsert,
                Command::InsertTab,
            ],
        );
        assert_eq!(text(&st), "he  llo");
        assert_eq!(st.cursor().0, 4);

        // On a tabstop boundary (vcol 4): a full unit is inserted, not zero.
        let st = run_indent(
            "hello",
            4,
            IndentStyle::Space,
            &[
                Command::Move(4, Motion::Right),
                Command::EnterInsert,
                Command::InsertTab,
            ],
        );
        assert_eq!(text(&st), "hell    o");
        assert_eq!(st.cursor().0, 8);
    }

    #[test]
    fn insert_tab_tab_style_inserts_a_hard_tab() {
        let st = run_indent(
            "hello",
            4,
            IndentStyle::Tab,
            &[
                Command::Move(2, Motion::Right),
                Command::EnterInsert,
                Command::InsertTab,
            ],
        );
        assert_eq!(text(&st), "he\tllo");
        assert_eq!(st.cursor().0, 3, "caret after the one-byte tab");
    }

    #[test]
    fn shift_right_leaves_a_truly_empty_line_untouched() {
        let st = run("", &[Command::ShiftRight(1)]);
        assert_eq!(text(&st), "", "Vim never indents an empty line");
        assert_eq!(st.cursor().0, 0);
    }

    #[test]
    fn shift_right_count_shifts_multiple_lines_cursor_stays_on_first() {
        let st = run("a\nb\nc", &[Command::ShiftRight(2)]);
        assert_eq!(
            text(&st),
            "    a\n    b\nc",
            "2>> shifts the first two lines"
        );
        assert_eq!(st.cursor().0, 0, "cursor stays on the first line, column 0");
    }

    #[test]
    fn shift_left_removes_one_level_of_spaces() {
        let st = run("    hello", &[Command::ShiftLeft(1)]);
        assert_eq!(text(&st), "hello");
        assert_eq!(st.cursor().0, 0);
    }

    #[test]
    fn shift_left_removes_at_most_one_level() {
        // 6 leading spaces, tab_width 4 -> removes 4, leaves 2.
        let st = run("      hi", &[Command::ShiftLeft(1)]);
        assert_eq!(text(&st), "  hi");
        assert_eq!(st.cursor().0, 0);
    }

    #[test]
    fn shift_left_partial_indent_never_crosses_column_zero() {
        let st = run("  hi", &[Command::ShiftLeft(1)]);
        assert_eq!(
            text(&st),
            "hi",
            "fewer than tab_width spaces: remove them all, no more"
        );
        assert_eq!(st.cursor().0, 0);
    }

    #[test]
    fn shift_left_on_unindented_line_is_a_noop() {
        let st = run("hi", &[Command::ShiftLeft(1)]);
        assert_eq!(text(&st), "hi");
        assert_eq!(st.cursor().0, 0);
    }

    #[test]
    fn shift_left_removes_a_leading_tab() {
        let st = run("\thello", &[Command::ShiftLeft(1)]);
        assert_eq!(text(&st), "hello");
        // The cursor on the tab wants its last column (3 at tabstop 4), and stays there.
        assert_eq!(st.cursor().0, 3);
    }

    #[test]
    fn shift_right_then_left_round_trips() {
        let st = run("hello", &[Command::ShiftRight(1), Command::ShiftLeft(1)]);
        assert_eq!(text(&st), "hello", ">> then << restores the original");
        assert_eq!(st.cursor().0, 0);
    }

    #[test]
    fn shift_is_undoable_as_one_edit() {
        let st = run("a\nb", &[Command::ShiftRight(2), Command::Undo]);
        assert_eq!(text(&st), "a\nb", "a single undo reverses the whole shift");
    }
}

#[cfg(test)]
mod format_tests {
    use crate::editor::*;

    fn run_tw(initial: &str, tw: usize, cmds: &[Command]) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        st.set_option(EditorOption::TextWidth(tw));
        for c in cmds {
            apply_command(&mut st, c);
        }
        st
    }

    fn text(st: &EditorState) -> String {
        String::from_utf8(st.bytes().to_vec()).expect("utf8")
    }

    #[test]
    fn gq_reflows_a_paragraph_to_textwidth() {
        // tw=10; `gqap` re-wraps the run of words so no line exceeds 10 columns.
        let st = run_tw(
            "aa bb cc dd ee ff\n",
            10,
            &[Command::Format {
                count: 1,
                motion: Motion::AParagraph,
                keep_cursor: false,
            }],
        );
        assert_eq!(text(&st), "aa bb cc\ndd ee ff\n");
        assert_eq!(st.mode(), Mode::Normal);
    }

    #[test]
    fn gq_preserves_indent_and_blank_line_separators() {
        // Two paragraphs; the first line's indent is applied to every wrapped line of that paragraph.
        let st = run_tw(
            "  one two three four\n\nfive six seven eight\n",
            12,
            &[Command::Format {
                count: 1,
                motion: Motion::AParagraph,
                keep_cursor: false,
            }],
        );
        // Only the FIRST paragraph is in `ap`'s span here; indent "  " kept, wrapped at 12 cols.
        assert_eq!(
            text(&st),
            "  one two\n  three four\n\nfive six seven eight\n"
        );
    }

    #[test]
    fn gw_keeps_the_cursor() {
        // `gw` reflows like `gq` but restores the caret roughly to where it was.
        let st = run_tw(
            "aa bb cc dd ee ff\n",
            10,
            &[
                Command::Move(4, Motion::Right),
                Command::Format {
                    count: 1,
                    motion: Motion::AParagraph,
                    keep_cursor: true,
                },
            ],
        );
        assert_eq!(text(&st), "aa bb cc\ndd ee ff\n");
        assert_eq!(st.cursor().0, 4, "gw restores the caret column");
    }

    #[test]
    fn rot13_over_a_motion_and_is_involutive() {
        use crate::command::WordCase;
        // `g?$` ROT13s to end of line: "Hello" -> "Uryyb".
        let st = run_tw(
            "Hello\n",
            0,
            &[Command::CaseMotion {
                count: 1,
                motion: Motion::LineEnd,
                case: WordCase::Rot13,
            }],
        );
        assert_eq!(text(&st), "Uryyb\n");
        // Applying ROT13 twice restores the original (involution); non-letters untouched.
        let st = run_tw(
            "aZ9!\n",
            0,
            &[
                Command::CaseMotion {
                    count: 1,
                    motion: Motion::LineEnd,
                    case: WordCase::Rot13,
                },
                Command::Move(1, Motion::LineStart),
                Command::CaseMotion {
                    count: 1,
                    motion: Motion::LineEnd,
                    case: WordCase::Rot13,
                },
            ],
        );
        assert_eq!(text(&st), "aZ9!\n");
    }

    #[test]
    fn textwidth_zero_falls_back_to_79() {
        // With tw=0, a short line under 79 cols is left as one line (no wrap).
        let st = run_tw(
            "short line of words\n",
            0,
            &[Command::Format {
                count: 1,
                motion: Motion::AParagraph,
                keep_cursor: false,
            }],
        );
        assert_eq!(text(&st), "short line of words\n");
    }

    /// #617: the options a trace header writes include the indent options, so replay does not depend
    /// on the replaying machine's config.
    #[test]
    fn editing_options_carry_the_indent_options() {
        let mut st = EditorState::new(b"x".to_vec());
        st.set_option(EditorOption::SoftTabStop(-1));
        st.set_option(EditorOption::AutoIndent(false));
        let opts = st.view.editing_options();
        assert!(opts.contains(&EditorOption::SoftTabStop(-1)));
        assert!(opts.contains(&EditorOption::AutoIndent(false)));
        assert!(opts.contains(&EditorOption::SmartTab(true)));
    }

    /// #618: the trace header records the formatting options, string ones included, and they round-trip
    /// through the `ex_set` codec.
    #[test]
    fn formatting_options_round_trip_through_the_trace_codec() {
        let mut st = EditorState::new(b"x".to_vec());
        st.set_option(EditorOption::Comments("b:#,://".into()));
        st.set_option(EditorOption::WrapMargin(-3));
        let opts = st.view.editing_options();
        assert!(opts.contains(&EditorOption::FormatOptions("tcqj".into())));
        assert!(opts.contains(&EditorOption::Comments("b:#,://".into())));
        assert!(opts.contains(&EditorOption::WrapMargin(-3)));
        for o in opts {
            let line = ExCmd::Set(o.clone()).to_line();
            let mut toks = line.splitn(2, ' ');
            let verb = toks.next().unwrap();
            let back = ExCmd::from_parts(verb, toks.next().unwrap_or(""), &line)
                .unwrap_or_else(|e| panic!("{line}: {e:?}"));
            assert_eq!(back, ExCmd::Set(o), "{line}");
        }
    }
}

/// Operator spans and their cursor rules (#693, #696, #706, #711, #712), beside the oracle rows.
mod operator_span_tests {
    use crate::command::{ForcedWise, OpKind, WordCase};
    use crate::editor::*;

    fn run(initial: &[u8], cmds: &[Command]) -> EditorState {
        let mut st = EditorState::new(initial.to_vec());
        for c in cmds {
            apply_command(&mut st, c);
        }
        st
    }

    #[test]
    fn recase_keeps_invalid_utf8_bytes() {
        // #706: `gUU` and `~` over a stray byte recase the letters around it instead of panicking.
        let st = run(
            b"a\xffb\n",
            &[Command::CaseMotion {
                count: 1,
                motion: Motion::Line,
                case: WordCase::Upcase,
            }],
        );
        assert_eq!(st.bytes(), b"A\xffB\n");
        let st = run(b"a\xffb\n", &[Command::ToggleCase(3)]);
        assert_eq!(st.bytes(), b"A\xffB\n");
    }

    #[test]
    fn x_and_r_take_a_whole_grapheme() {
        // #696: a base with its combining mark and a ZWJ emoji sequence are one char.
        let st = run("e\u{301}x\n".as_bytes(), &[Command::DeleteUnder(1)]);
        assert_eq!(st.bytes(), b"x\n");
        assert_eq!(st.register().text(), "e\u{301}".as_bytes());
        let st = run(
            "\u{1F469}\u{200D}\u{1F4BB}x\n".as_bytes(),
            &[Command::DeleteUnder(1)],
        );
        assert_eq!(st.bytes(), b"x\n");
        let st = run("e\u{301}b\n".as_bytes(), &[Command::ReplaceChar(1, 'Z')]);
        assert_eq!(st.bytes(), b"Zb\n");
        let st = run("e\u{301}b\n".as_bytes(), &[Command::ReplaceChar(2, 'Z')]);
        assert_eq!(st.bytes(), b"ZZ\n");
        // Deleting the last char leaves the cursor at the start of the cluster before it, not inside it.
        let st = run(
            "e\u{301}b".as_bytes(),
            &[Command::Move(1, Motion::Right), Command::DeleteUnder(1)],
        );
        assert_eq!(st.cursor().0, 0);
    }

    #[test]
    fn change_over_a_failed_linewise_motion_stays_in_normal() {
        // #693: `cj` on the last line and `3cc` there fail like `dj`; `cc` on an empty buffer still enters
        // Insert.
        let down = Command::Move(1, Motion::Down);
        let st = run(b"a\nb", &[down.clone(), Command::Change(1, Motion::Down)]);
        assert_eq!(st.mode(), Mode::Normal);
        assert_eq!(st.bytes(), b"a\nb");
        let st = run(b"a\nb", &[down, Command::Change(3, Motion::Line)]);
        assert_eq!(st.mode(), Mode::Normal);
        let st = run(b"", &[Command::Change(1, Motion::Line)]);
        assert_eq!(st.mode(), Mode::Insert);
    }

    #[test]
    fn linewise_yank_and_delete_keep_the_wanted_column() {
        // #712: `wyy` stays on `d`; `jwyk` rests on line 1 at the same column.
        let st = run(
            b"abc def\nghi jkl\n",
            &[
                Command::Move(1, Motion::WordFwd),
                Command::Yank(1, Motion::Line),
            ],
        );
        assert_eq!(st.cursor().0, 4);
        let st = run(
            b"abc def\nghi jkl\n",
            &[
                Command::Move(1, Motion::Down),
                Command::Move(1, Motion::WordFwd),
                Command::Yank(1, Motion::Up),
            ],
        );
        assert_eq!(st.cursor().0, 4);
        // `ldd` on an indented line keeps column 1 on the line that moves up ('nostartofline').
        let st = run(
            b"  x\n  abc\n",
            &[
                Command::Move(1, Motion::Right),
                Command::Delete(1, Motion::Line),
            ],
        );
        assert_eq!(st.cursor().0, 1);
    }

    #[test]
    fn forced_charwise_toggles_inclusive_motions() {
        // #711: `dvg_` leaves the motion's end char; `ldvG` reaches the kept column; `dVw` from a line's
        // last word takes only that line.
        let forced = |m, wise| Command::OpForced {
            op: OpKind::Delete,
            count: 1,
            motion: m,
            wise,
        };
        let st = run(
            b"abc  ",
            &[forced(Motion::LineLastNonBlank, ForcedWise::Charwise)],
        );
        assert_eq!(st.bytes(), b"c  ");
        let st = run(
            b"ab\ncd",
            &[
                Command::Move(1, Motion::Right),
                forced(Motion::LastLine, ForcedWise::Charwise),
            ],
        );
        assert_eq!(st.bytes(), b"ad");
        let st = run(
            b"abc\ndef",
            &[forced(Motion::WordFwd, ForcedWise::Linewise)],
        );
        assert_eq!(st.bytes(), b"def");
    }

    /// The cursor after a linewise operator that `"/` refuses (#1174), as nvim v0.12.6 leaves it: a delete at
    /// the wanted column on the range's first line, a yank at the range's start.
    #[test]
    fn a_refused_linewise_operator_stays_at_the_start_of_its_range() {
        const WORDS: &[u8] = b"one two three\nfour five six\nseven eight\n";
        let at = |cmds: &[Command], op: Command| {
            let mut st = EditorState::new(WORDS.to_vec());
            for c in cmds {
                apply_command(&mut st, c);
            }
            apply_command(&mut st, &Command::SetRegister(Some('/')));
            apply_command(&mut st, &op);
            assert_eq!(st.bytes(), WORDS, "{op:?}: nothing is deleted");
            st.cursor().0
        };
        let line2_w = [
            Command::Move(1, Motion::Down),
            Command::Move(1, Motion::WordFwd),
        ];
        let line3_w = [
            Command::Move(1, Motion::LastLine),
            Command::Move(1, Motion::WordFwd),
        ];
        // `jw"/dd` keeps [2,5]; `jw"/dk` goes up to [1,5]; `jw"/yk` too.
        assert_eq!(at(&line2_w, Command::Delete(1, Motion::Line)), 14 + 5);
        assert_eq!(at(&line2_w, Command::Delete(1, Motion::Up)), 5);
        assert_eq!(at(&line2_w, Command::Yank(1, Motion::Up)), 5);
        // On the last line, whose delete span starts at the newline before it: `Gw"/dd` keeps [3,6].
        assert_eq!(at(&line3_w, Command::Delete(1, Motion::Line)), 28 + 6);
        assert_eq!(at(&line3_w, Command::Delete(1, Motion::Up)), 14 + 6);
        // `Gw"/dgg` lands at the wanted column 6 on line 1.
        assert_eq!(at(&line3_w, Command::Delete(1, Motion::GotoLine)), 6);
    }
}
