//! Registers and `:put` of lines. Split from `editor/tests.rs` (#1397).

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
        assert_eq!(st.cursor().0, 3, "cursor lands after the inserted text");
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
    fn special_registers_insert_and_paste_from_synced_values() {
        // `"/ ": ". "%` resolve from the frontend-synced values (`:help quote_/`). Build a fresh state
        // (EditorState is not Clone) with the four slots synced.
        let synced = || {
            let mut st = EditorState::new(b"xy".to_vec());
            st.set_special_registers(
                Some("pat".into()),
                Some("wq".into()),
                Some("hello".into()),
                Some("src/main.rs".into()),
            );
            st
        };
        // `<C-r>/` inserts the last search pattern at the caret (charwise, staying in Insert).
        let mut st_ins = synced();
        apply_command(&mut st_ins, &Command::EnterInsert);
        apply_command(&mut st_ins, &Command::InsertRegister('/'));
        assert_eq!(
            text(&st_ins),
            "patxy",
            "<C-r>/ inserts the last search pattern"
        );
        assert_eq!(st_ins.mode(), Mode::Insert);

        // `"%p` pastes the file name after the caret (charwise). Arm `"%` then paste.
        let mut st_pct = synced();
        apply_command(&mut st_pct, &Command::SetRegister(Some('%')));
        apply_command(
            &mut st_pct,
            &Command::Paste {
                after: true,
                count: 1,
                move_after: false,
            },
        );
        assert_eq!(text(&st_pct), "xsrc/main.rsy", "\"%p pastes the file name");

        // `":p` reads its slot too.
        let mut st_colon = synced();
        apply_command(&mut st_colon, &Command::SetRegister(Some(':')));
        apply_command(
            &mut st_colon,
            &Command::Paste {
                after: true,
                count: 1,
                move_after: false,
            },
        );
        assert_eq!(text(&st_colon), "xwqy", "\":p pastes the last Ex line");
    }

    #[test]
    fn special_registers_are_read_only_from_yank_and_paste_back() {
        // A yank NAMING `"/` is swallowed: the slot keeps its synced value and the unnamed register is
        // untouched (read-only, `:help quote_/`). Then `"/p` still pastes the original pattern.
        let mut st = EditorState::new(b"abc\n".to_vec());
        st.set_special_registers(Some("pat".into()), None, None, None);
        // `"/yy` — arm "/ and yank the line into it (a no-op write).
        apply_command(&mut st, &Command::SetRegister(Some('/')));
        apply_command(&mut st, &Command::Yank(1, Motion::Line));
        assert_eq!(
            st.registers().get(Some('/')).text(),
            b"pat",
            "yank into \"/ is swallowed"
        );
        assert!(
            st.registers().unnamed().is_empty(),
            "a read-only-register yank never touches unnamed"
        );
        // `"/p` pastes the still-intact pattern after the caret.
        apply_command(&mut st, &Command::SetRegister(Some('/')));
        apply_command(
            &mut st,
            &Command::Paste {
                after: true,
                count: 1,
                move_after: false,
            },
        );
        assert_eq!(text(&st), "apatbc\n");
    }

    #[test]
    fn expr_register_arms_the_next_paste() {
        // `"=3.0/2<CR>` arms the `"=` register; the following `p` pastes the evaluated "1.5".
        let st = run(
            "ab",
            &[
                Command::SetExprRegister("3.0/2".into()),
                Command::Paste {
                    after: true,
                    count: 1,
                    move_after: false,
                },
            ],
        );
        assert_eq!(
            text(&st),
            "a1.5b",
            "the evaluated result pastes after the cursor"
        );
        assert_eq!(
            st.registers().get(Some('=')).text(),
            b"1.5",
            "the \"= register holds the evaluated result"
        );
    }

    #[test]
    fn expr_register_empty_result_makes_paste_a_noop() {
        // A malformed `"=` expression stores an empty result, so the following paste does nothing.
        let st = run(
            "ab",
            &[
                Command::SetExprRegister("bogus(".into()),
                Command::Paste {
                    after: true,
                    count: 1,
                    move_after: false,
                },
            ],
        );
        assert_eq!(text(&st), "ab", "a broken expression pastes nothing");
        assert!(
            st.registers().get(Some('=')).is_empty(),
            "the \"= register is empty after an evaluation error"
        );
    }

    #[test]
    fn blackhole_delete_preserves_the_yank_register() {
        // `yy` into unnamed, then `"_dd` deletes a MIDDLE line WITHOUT clobbering the yank; `p` still pastes it.
        let st = run(
            "keep\ntrash\nlast\n",
            &[
                Command::Yank(1, Motion::Line),   // unnamed = "keep\n"
                Command::Move(1, Motion::Down),   // to "trash"
                Command::SetRegister(Some('_')),  // "_
                Command::Delete(1, Motion::Line), // "_dd — discarded; cursor drops to "last"
                Command::Paste {
                    after: true,
                    count: 1,
                    move_after: false,
                },
            ],
        );
        // "trash" was deleted into the blackhole; the paste re-inserts the still-intact "keep" below "last".
        assert_eq!(text(&st), "keep\nlast\nkeep\n");
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
            gp.cursor().0,
            p.cursor().0 + 1,
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
    fn linewise_paste_rests_cursor_on_first_non_blank() {
        // Yank an indented line, paste it below: the cursor lands on the first non-blank ('f'), not col 0.
        let st = run(
            "    foo\nbar",
            &[
                Command::Yank(1, Motion::Line),
                Command::Paste {
                    after: true,
                    count: 1,
                    move_after: false,
                },
            ],
        );
        assert_eq!(text(&st), "    foo\n    foo\nbar");
        assert_eq!(
            st.cursor().0,
            12,
            "on the 'f' of the pasted line, past its indent"
        );

        // `P` (above) with indent lands on the first non-blank of the pasted line at the top.
        let st = run(
            "    foo\nbar",
            &[
                Command::Yank(1, Motion::Line),
                Command::Paste {
                    after: false,
                    count: 1,
                    move_after: false,
                },
            ],
        );
        assert_eq!(text(&st), "    foo\n    foo\nbar");
        assert_eq!(st.cursor().0, 4, "on the 'f' of the pasted-above line");
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
        assert_eq!(st.cursor().0, 2, "cursor lands on the last pasted byte");
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
        assert_eq!(st.cursor().0, 9);
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
        assert_eq!(
            st.cursor().0,
            2,
            "cursor sits at the end of the kept indent"
        );
        assert_eq!(st.mode(), Mode::Insert);
        assert!(st.register().is_linewise());
        assert_eq!(st.register().text(), b"  hello\n");
    }
}

/// `:[line]put [reg]` — the ex put command. Put is ALWAYS LINEWISE: a charwise register's text is split on
/// newlines and each piece inserted as its own whole line. Ground truth captured from nvim v0.12.4
/// (`nvim -u NONE`, `writefile(getline(1,'$'))` + `line('.')` / `col('.')`).
#[cfg(test)]
mod put_lines_tests {
    use crate::editor::*;
    use crate::register::Register;

    fn text(st: &EditorState) -> String {
        String::from_utf8(st.doc.bytes().to_vec()).unwrap()
    }

    /// The cursor's 0-based (line, column).
    fn cursor_line_col(st: &EditorState) -> (usize, usize) {
        let b = st.doc.bytes();
        let c = st.cursor().0;
        let line = b[..c].iter().filter(|&&x| x == b'\n').count();
        let line_start = b[..c]
            .iter()
            .rposition(|&x| x == b'\n')
            .map_or(0, |i| i + 1);
        (line, c - line_start)
    }

    /// A linewise register is put as whole line(s) BELOW the addressed line; the cursor lands on the last
    /// inserted line. nvim: yank line1, `:2put` → `line1/line2/line1/line3`, cursor (3,1).
    #[test]
    fn linewise_put_below_addressed_line() {
        let mut st = EditorState::new(b"line1\nline2\nline3\n".to_vec());
        assert_eq!(
            st.yank_lines(SubRange::Lines(1, 1)),
            1,
            "yank line1 linewise"
        );
        assert_eq!(
            st.put_lines(LineAddr::Line(2), None),
            1,
            ":2put inserts one line"
        );
        assert_eq!(text(&st), "line1\nline2\nline1\nline3\n");
        assert_eq!(cursor_line_col(&st), (2, 0), "cursor on the inserted line");
    }

    /// The linewise-FORCING rule: a CHARWISE register is still put as its own new whole line — the key
    /// difference from normal-mode `p`. nvim: reg a='foo' (charwise), `:2put a` → a new `foo` line.
    #[test]
    fn charwise_register_is_forced_linewise() {
        let mut st = EditorState::new(b"line1\nline2\nline3\n".to_vec());
        st.view
            .registers
            .yank(Some('a'), Register::charwise(b"foo".to_vec()));
        assert_eq!(st.put_lines(LineAddr::Line(2), Some('a')), 1);
        assert_eq!(
            text(&st),
            "line1\nline2\nfoo\nline3\n",
            "charwise `foo` opens a whole new line, not an inline splice"
        );
        assert_eq!(cursor_line_col(&st), (2, 0));
    }

    /// A charwise register carrying newlines splits into one line PER newline-separated piece. nvim: reg
    /// a="aa\nbb" (charwise), `:1put a` → two new lines `aa` and `bb`, cursor on `bb` (3,1).
    #[test]
    fn charwise_register_with_newlines_splits_into_lines() {
        let mut st = EditorState::new(b"line1\nline2\nline3\n".to_vec());
        st.view
            .registers
            .yank(Some('a'), Register::charwise(b"aa\nbb".to_vec()));
        assert_eq!(st.put_lines(LineAddr::Line(1), Some('a')), 2, "two lines");
        assert_eq!(text(&st), "line1\naa\nbb\nline2\nline3\n");
        assert_eq!(
            cursor_line_col(&st),
            (2, 0),
            "cursor on the last inserted line"
        );
    }

    /// A register written with nothing puts one empty line; one never written puts nothing (#1342). nvim
    /// v0.12.6: `qaq:put a` and `:put =''` give `line1` and an empty line; `:put b` of an unset `b` is E353.
    #[test]
    fn a_written_empty_register_puts_one_empty_line() {
        let mut st = EditorState::new(b"line1\n".to_vec());
        assert_eq!(
            st.put_lines(LineAddr::Line(1), Some('b')),
            0,
            "never written"
        );
        assert_eq!(text(&st), "line1\n");
        st.view
            .registers
            .set_slot(Some('a'), Register::charwise(Vec::new()));
        assert_eq!(st.put_lines(LineAddr::Line(1), Some('a')), 1);
        assert_eq!(text(&st), "line1\n\n");
        st.view
            .registers
            .set_slot(Some('='), Register::expr_result(b"z".to_vec()));
        assert_eq!(st.put_lines(LineAddr::Line(0), Some('=')), 1);
        assert_eq!(text(&st), "z\nline1\n\n");
    }

    /// `:0put` inserts at the very top, before line 1. nvim: yank line1, `:0put` → `line1/line1/line2/line3`,
    /// cursor (1,1).
    #[test]
    fn put_at_line_zero_inserts_at_the_top() {
        let mut st = EditorState::new(b"line1\nline2\nline3\n".to_vec());
        st.yank_lines(SubRange::Lines(1, 1));
        assert_eq!(st.put_lines(LineAddr::Line(0), None), 1);
        assert_eq!(text(&st), "line1\nline1\nline2\nline3\n");
        assert_eq!(cursor_line_col(&st), (0, 0), "cursor on the new top line");
    }

    /// `:$put` inserts after the last line. nvim: yank line1, `:$put` → `line1/line2/line3/line1`,
    /// cursor (4,1).
    #[test]
    fn put_at_last_line_appends() {
        let mut st = EditorState::new(b"line1\nline2\nline3\n".to_vec());
        st.yank_lines(SubRange::Lines(1, 1));
        assert_eq!(st.put_lines(LineAddr::Last, None), 1);
        assert_eq!(text(&st), "line1\nline2\nline3\nline1\n");
        assert_eq!(cursor_line_col(&st), (3, 0));
    }

    /// A bare `:put` (LineAddr::Current) puts after the cursor's line. nvim: cursor on line1, yank line1,
    /// `:put` → `line1/line1/line2/line3`, cursor (2,1).
    #[test]
    fn bare_put_inserts_after_the_current_line() {
        let mut st = EditorState::new(b"line1\nline2\nline3\n".to_vec());
        st.set_cursor(BytePos(0)); // line1
        st.yank_lines(SubRange::Lines(1, 1));
        assert_eq!(st.put_lines(LineAddr::Current, None), 1);
        assert_eq!(text(&st), "line1\nline1\nline2\nline3\n");
        assert_eq!(cursor_line_col(&st), (1, 0));
    }

    /// The cursor lands on the FIRST NON-BLANK of the last inserted line, not column 0. nvim: reg
    /// a="   indented\n" (linewise), `:1put a` → cursor (2,4).
    #[test]
    fn cursor_lands_on_first_non_blank() {
        let mut st = EditorState::new(b"line1\nline2\nline3\n".to_vec());
        st.view
            .registers
            .yank(Some('a'), Register::linewise(b"   indented\n".to_vec()));
        assert_eq!(st.put_lines(LineAddr::Line(1), Some('a')), 1);
        assert_eq!(text(&st), "line1\n   indented\nline2\nline3\n");
        assert_eq!(
            cursor_line_col(&st),
            (1, 3),
            "cursor on the first non-blank (past the 3 leading spaces)"
        );
    }

    /// An empty register is a no-op: the buffer and cursor are untouched. nvim: `:1put z` (z empty) leaves
    /// the buffer unchanged.
    #[test]
    fn empty_register_is_a_no_op() {
        let mut st = EditorState::new(b"line1\nline2\nline3\n".to_vec());
        st.set_cursor(BytePos(6)); // line2
        assert_eq!(
            st.put_lines(LineAddr::Line(1), Some('z')),
            0,
            "nothing inserted"
        );
        assert_eq!(text(&st), "line1\nline2\nline3\n", "buffer untouched");
        assert_eq!(cursor_line_col(&st), (1, 0), "cursor untouched");
    }

    /// A put is ONE undo group: a single undo removes all inserted lines.
    #[test]
    fn put_is_one_undo_group() {
        let mut st = EditorState::new(b"line1\nline2\nline3\n".to_vec());
        st.view
            .registers
            .yank(Some('a'), Register::charwise(b"aa\nbb".to_vec()));
        st.put_lines(LineAddr::Line(1), Some('a'));
        assert_eq!(text(&st), "line1\naa\nbb\nline2\nline3\n");
        apply_command(&mut st, &Command::Undo);
        assert_eq!(
            text(&st),
            "line1\nline2\nline3\n",
            "one undo removes the whole put"
        );
    }
}
