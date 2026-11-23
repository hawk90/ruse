//! Visual, blockwise and Select modes and their operators. Split from `editor/tests.rs` (#1397).

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
    fn visual_linewise_change_replaces_lines_with_one_and_keeps_the_separator() {
        // `Vjc` over "abc\nbeta\ngamma\n": the two selected WHOLE lines collapse to ONE empty line, the
        // separator to `gamma` is PRESERVED (never merged in), and typing `X` then leaving Insert yields
        // "X\ngamma\n" with the caret on the new line — exactly `cc`/`2cc` over the same range (nvim
        // v0.12.4, issue #435). Regression guard: the trailing '\n' before `gamma` must survive.
        let st = run(
            "abc\nbeta\ngamma\n",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Linewise,
                },
                Command::Move(1, Motion::Down),
                Command::ChangeSelection,
            ],
        );
        // After `Vjc` (before typing): both lines gone, ONE empty line remains, `gamma` intact below it.
        assert_eq!(
            text(&st),
            "\ngamma\n",
            "one empty line replaces the two; gamma kept"
        );
        assert_eq!(st.cursor().0, 0, "caret on the new empty line");
        assert_eq!(st.mode(), Mode::Insert, "change enters Insert");
        assert!(
            st.register().is_linewise(),
            "selected lines captured linewise"
        );
        assert_eq!(
            st.register().text(),
            b"abc\nbeta\n",
            "whole lines incl. trailing newline"
        );

        // Type `X`, leave Insert → the final "X\ngamma\n" ground truth.
        let mut st = st;
        apply_command(&mut st, &Command::InsertChar('X'));
        apply_command(&mut st, &Command::EnterNormal);
        assert_eq!(text(&st), "X\ngamma\n", "final bytes match nvim VjcX<Esc>");
        assert_eq!(st.cursor().0, 0, "caret on the changed line's only char");
    }

    #[test]
    fn visual_linewise_change_preserves_leading_indent() {
        // Like `cc`, visual-linewise change keeps the FIRST line's leading indent and drops Insert after it.
        let st = run(
            "  hello\nworld\n",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Linewise,
                },
                Command::ChangeSelection,
            ],
        );
        assert_eq!(
            text(&st),
            "  \nworld\n",
            "leading indent survives visual-linewise change"
        );
        assert_eq!(st.cursor().0, 2, "caret at the end of the kept indent");
        assert_eq!(st.mode(), Mode::Insert);
        assert!(st.register().is_linewise());
        assert_eq!(st.register().text(), b"  hello\n");
    }

    #[test]
    fn visual_charwise_change_does_not_regress() {
        // `vjc` (CHARWISE, not linewise) over "abc\nbeta\ngamma\n" deletes the inclusive charwise span
        // `[cur, downtarget]` and stays a plain delete-then-Insert — it must NOT route through cc-logic.
        // nvim: `vjcX<Esc>` → "Xeta\ngamma\n".
        let st = run(
            "abc\nbeta\ngamma\n",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Charwise,
                },
                Command::Move(1, Motion::Down),
                Command::ChangeSelection,
                Command::InsertChar('X'),
                Command::EnterNormal,
            ],
        );
        assert_eq!(
            text(&st),
            "Xeta\ngamma\n",
            "charwise change unchanged by the fix"
        );
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

    // #727, nvim v0.12.6 (`--clean`): fixtures r727_* in tools/parity/oracle.py.
    #[test]
    fn replace_cr_stays_in_replace_and_bs_joins_it_back() {
        // `lRX<CR>Y<Esc>` on "abcdef" → "aX" / "Ydef": the new line is still overwritten.
        let st = run(
            "abcdef",
            &[
                Command::Move(1, Motion::Right),
                Command::EnterReplace,
                Command::ReplaceType('X'),
                Command::InsertNewline,
                Command::ReplaceType('Y'),
                Command::EnterNormal,
            ],
        );
        assert_eq!(text(&st), "aX\nYdef");
        // `RX<CR><BS><BS><Esc>` on "abc" → "abc": the first <BS> joins, the second restores `a`.
        let st = run(
            "abc",
            &[
                Command::EnterReplace,
                Command::ReplaceType('X'),
                Command::InsertNewline,
                Command::ReplaceBackspace,
                Command::ReplaceBackspace,
                Command::EnterNormal,
            ],
        );
        assert_eq!(text(&st), "abc");
        // `lRX<CR><BS><BS><Esc>` on "ab  cd" → "ab  cd": the blanks the break dropped come back.
        let st = run(
            "ab  cd",
            &[
                Command::Move(1, Motion::Right),
                Command::EnterReplace,
                Command::ReplaceType('X'),
                Command::InsertNewline,
                Command::ReplaceBackspace,
                Command::ReplaceBackspace,
                Command::EnterNormal,
            ],
        );
        assert_eq!(text(&st), "ab  cd");
    }

    #[test]
    fn replace_cr_autoindents_and_smarttab_bs_takes_the_indent_back() {
        // `wlRX<CR>Y<Esc>` on "  abcdef" → "  aX" / "  Ydef"; `wlRX<CR><BS><BS><Esc>` → "  aXcdef".
        let lead = [
            Command::Move(1, Motion::WordFwd),
            Command::Move(1, Motion::Right),
        ];
        let typed = |tail: &[Command]| {
            let mut cmds = lead.to_vec();
            cmds.extend([
                Command::EnterReplace,
                Command::ReplaceType('X'),
                Command::InsertNewline,
            ]);
            cmds.extend_from_slice(tail);
            cmds.push(Command::EnterNormal);
            text(&run("  abcdef", &cmds))
        };
        assert_eq!(typed(&[Command::ReplaceType('Y')]), "  aX\n  Ydef");
        assert_eq!(typed(&[Command::ReplaceBackspace]), "  aX\ncdef");
        assert_eq!(
            typed(&[Command::ReplaceBackspace, Command::ReplaceBackspace]),
            "  aXcdef"
        );
    }

    #[test]
    fn a_caret_jump_in_replace_starts_a_fresh_bs_run() {
        // `Rxy<Left><Left><BS><Esc>` on "abcd" → "xycd": nothing typed before the jump comes back.
        let st = run(
            "abcd",
            &[
                Command::EnterReplace,
                Command::ReplaceType('x'),
                Command::ReplaceType('y'),
                Command::Move(1, Motion::Left),
                Command::Move(1, Motion::Left),
                Command::ReplaceBackspace,
                Command::EnterNormal,
            ],
        );
        assert_eq!(text(&st), "xycd");
        // `Rxy<Left>Z<BS><Esc>` → "xycd": what is typed after the jump still comes back.
        let st = run(
            "abcd",
            &[
                Command::EnterReplace,
                Command::ReplaceType('x'),
                Command::ReplaceType('y'),
                Command::Move(1, Motion::Left),
                Command::ReplaceType('Z'),
                Command::ReplaceBackspace,
                Command::EnterNormal,
            ],
        );
        assert_eq!(text(&st), "xycd");
        assert_eq!(st.cursor(), BytePos(0));
    }

    #[test]
    fn virtual_replace_cr_moves_onto_the_next_line() {
        // `lgRX<CR>Y<Esc>` on "abcdef" / "ghij" → "aX" / "Yhij": the rest of the line goes, the next
        // line is overwritten; `lgRX<CR><BS><Esc>` → "aXcdef" / "ghij"; on the last line a line is added.
        let typed = |initial: &str, tail: &[Command]| {
            let mut cmds = vec![
                Command::Move(1, Motion::Right),
                Command::EnterVirtualReplace,
                Command::VirtualReplaceType('X'),
                Command::InsertNewline,
            ];
            cmds.extend_from_slice(tail);
            cmds.push(Command::EnterNormal);
            text(&run(initial, &cmds))
        };
        let y = Command::VirtualReplaceType('Y');
        assert_eq!(typed("abcdef\nghij", std::slice::from_ref(&y)), "aX\nYhij");
        assert_eq!(
            typed("abcdef\nghij", &[Command::ReplaceBackspace]),
            "aXcdef\nghij"
        );
        let bs = Command::ReplaceBackspace;
        assert_eq!(
            typed("abcdef\nghij", &[y.clone(), bs.clone(), bs.clone(), bs]),
            "abcdef\nghij"
        );
        assert_eq!(typed("abcdef\n", &[y]), "aX\nY\n");
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
            "ab\n",
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
        assert_eq!(st.cursor().0, 0);
        assert_eq!(st.register().text(), b"hello");
        assert!(!st.register().is_linewise());
        assert_eq!(st.mode(), Mode::Normal);
    }

    #[test]
    fn visual_inner_paragraph_is_linewise() {
        // `vip` selects the paragraph and switches the selection to LINEWISE (Vim), so `vipd` yields a
        // linewise register — verified against nvim v0.12.4 (oracle fixture v_ip_paragraph).
        let st = run(
            "a\nb\n\nc",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Charwise,
                },
                Command::Move(1, Motion::InnerParagraph),
                Command::DeleteSelection,
            ],
        );
        assert_eq!(text(&st), "\nc");
        assert!(st.register().is_linewise(), "vip delete is linewise");
        assert_eq!(st.register().text(), b"a\nb\n");
        // A charwise object (`iw`) in Visual stays charwise (regression guard).
        let st = run(
            "foo bar",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Charwise,
                },
                Command::Move(1, Motion::InnerWord),
                Command::DeleteSelection,
            ],
        );
        assert!(!st.register().is_linewise(), "viw stays charwise");
    }

    #[test]
    fn gv_without_a_prior_selection_is_a_noop() {
        let st = run("abc", &[Command::ReselectVisual]);
        assert_eq!(text(&st), "abc");
        assert_eq!(st.mode(), Mode::Normal);
        assert_eq!(st.cursor().0, 0);
    }

    // Build the last-visual store by making a selection and leaving it (Esc = EnterNormal). Shared setup.
    fn select_then_normal(initial: &str, sel: &[Command]) -> EditorState {
        let mut cmds: Vec<Command> = sel.to_vec();
        cmds.push(Command::EnterNormal);
        run(initial, &cmds)
    }

    #[test]
    fn visual_marks_charwise_land_on_word_start_and_end() {
        // `viw` on the second word of "foo bar baz" selects "bar" (bytes 4..=6); after Esc, `` `< `` lands on
        // 'b' (4) and `` `> `` on 'r' (6). Verified against nvim v0.12.4 (visual_mark_*_charwise fixtures).
        let sel = [
            Command::Move(1, Motion::WordFwd),
            Command::EnterVisual {
                kind: SelectKind::Charwise,
            },
            Command::Move(1, Motion::InnerWord),
        ];
        let mut st = select_then_normal("foo bar baz", &sel);
        apply_command(&mut st, &Command::GotoVisualMarkStart);
        assert_eq!(st.cursor().0, 4, "`< on word start");
        apply_command(&mut st, &Command::GotoVisualMarkEnd);
        assert_eq!(st.cursor().0, 6, "`> on word end (inclusive last char)");
    }

    #[test]
    fn visual_marks_linewise_columns_and_lines() {
        // `Vj` on "  alpha\nbeta\n  gamma" selects lines 1..2. After Esc:
        //   `'<` = first non-blank of line 1 ('a' at 2), `'>` = first non-blank of line 2 ('b' at 8),
        //   `` `< `` = col 0 of line 1 (0), `` `> `` = last char of line 2 ('a' at 11). (nvim v0.12.4.)
        let sel = [
            Command::EnterVisual {
                kind: SelectKind::Linewise,
            },
            Command::Move(1, Motion::Down),
        ];
        let text = "  alpha\nbeta\n  gamma";
        let mut st = select_then_normal(text, &sel);
        apply_command(&mut st, &Command::GotoVisualMarkStartLine);
        assert_eq!(st.cursor().0, 2, "'< first non-blank of first line");
        apply_command(&mut st, &Command::GotoVisualMarkEndLine);
        assert_eq!(st.cursor().0, 8, "'> first non-blank of last line");
        apply_command(&mut st, &Command::GotoVisualMarkStart);
        assert_eq!(st.cursor().0, 0, "`< col 0 of first line (linewise)");
        apply_command(&mut st, &Command::GotoVisualMarkEnd);
        assert_eq!(st.cursor().0, 11, "`> last char of last line (linewise)");
    }

    #[test]
    fn visual_marks_set_after_an_operator() {
        // `viwd` deletes "bar" from "foo bar baz" -> "foo  baz" and leaves Visual by COMPLETING the operator.
        // The `<`/`>` marks still reflect the just-deleted region's columns (4 and 6), matching nvim exactly.
        let st_setup = [
            Command::Move(1, Motion::WordFwd),
            Command::EnterVisual {
                kind: SelectKind::Charwise,
            },
            Command::Move(1, Motion::InnerWord),
            Command::DeleteSelection,
        ];
        let mut st = run("foo bar baz", &st_setup);
        assert_eq!(text(&st), "foo  baz");
        apply_command(&mut st, &Command::GotoVisualMarkStart);
        assert_eq!(st.cursor().0, 4, "`< around the deleted region");
        apply_command(&mut st, &Command::GotoVisualMarkEnd);
        assert_eq!(st.cursor().0, 6, "`> around the deleted region");
    }

    #[test]
    fn visual_marks_blockwise_are_the_two_corners() {
        // A block from (row0,col1) to (row1,col2) over "abcde\nfghij\nklmno": `< = top corner (byte 1),
        // `> = bottom corner (byte 8 = 'h'). Byte-min/max of the two ends, as nvim reports the corners.
        let sel = [
            Command::Move(1, Motion::Right),
            Command::EnterVisual {
                kind: SelectKind::Blockwise,
            },
            Command::Move(1, Motion::Down),
            Command::Move(1, Motion::Right),
        ];
        let mut st = select_then_normal("abcde\nfghij\nklmno", &sel);
        apply_command(&mut st, &Command::GotoVisualMarkStart);
        assert_eq!(st.cursor().0, 1, "`< top corner");
        apply_command(&mut st, &Command::GotoVisualMarkEnd);
        assert_eq!(st.cursor().0, 8, "`> bottom corner");
    }

    #[test]
    fn visual_marks_are_unset_before_any_selection() {
        // Every visual-mark jump is a clean no-op before the first Visual exit (cursor stays put).
        for cmd in [
            Command::GotoVisualMarkStart,
            Command::GotoVisualMarkEnd,
            Command::GotoVisualMarkStartLine,
            Command::GotoVisualMarkEndLine,
        ] {
            let mut st = run("hello", &[Command::Move(1, Motion::Right)]);
            let before = st.cursor().0;
            apply_command(&mut st, &cmd);
            assert_eq!(st.cursor().0, before, "no-op before any visual selection");
            assert_eq!(text(&st), "hello");
        }
    }

    #[test]
    fn gv_restores_linewise_kind() {
        // `gv` restores the selection's KIND: after a linewise `Vj` selection (yanked, leaving Visual),
        // `gv` re-enters LINEWISE Visual so `gvd` deletes whole lines. Guards kind round-trip (nvim parity).
        let st = run(
            "one\ntwo\nthree",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Linewise,
                },
                Command::Move(1, Motion::Down),
                Command::YankSelection,
                Command::ReselectVisual,
                Command::DeleteSelection,
            ],
        );
        assert_eq!(text(&st), "three");
        assert!(st.register().is_linewise(), "gv kept the linewise kind");
    }

    #[test]
    fn visual_mark_jump_does_not_disturb_the_gv_store() {
        // A `` `< `` jump before `gv` reads the SAME store `gv` restores from, and leaves it intact: after
        // `viw<Esc>`<`, a following `gvd` still deletes the original word (nvim gv_after_mark_jump fixture).
        let st = run(
            "foo bar baz",
            &[
                Command::Move(1, Motion::WordFwd),
                Command::EnterVisual {
                    kind: SelectKind::Charwise,
                },
                Command::Move(1, Motion::InnerWord),
                Command::EnterNormal,
                Command::GotoVisualMarkStart,
                Command::ReselectVisual,
                Command::DeleteSelection,
            ],
        );
        assert_eq!(text(&st), "foo  baz");
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
        let cur_before = base.cursor().0;

        let mut st = base;
        apply_command(&mut st, &Command::SwapSelectionEnds);
        apply_command(&mut st, &Command::SwapSelectionEnds);
        assert_eq!(st.selection_span(), span_before, "oo restores the span");
        assert_eq!(st.cursor().0, cur_before, "oo restores the active end");
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
        assert_eq!(st.cursor().0, 0);
        assert_eq!(st.mode(), Mode::Normal);
        assert!(st.selection_span().is_none());
    }
}

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
fn visual_case_recases_the_selection() {
    use crate::command::WordCase;
    // `v` + 2×l selects "hel"; `U` uppercases it, cursor back to the start, Normal.
    let st = run(
        "hello",
        &[
            Command::EnterVisual {
                kind: SelectKind::Charwise,
            },
            Command::MoveRight,
            Command::MoveRight,
            Command::CaseSelection(WordCase::Upcase),
        ],
    );
    assert_eq!(text(&st), "HELlo");
    assert_eq!(st.mode(), Mode::Normal);
    assert_eq!(st.cursor().0, 0);
    // `u` lowercases.
    let st = run(
        "HELLO",
        &[
            Command::EnterVisual {
                kind: SelectKind::Charwise,
            },
            Command::MoveRight,
            Command::CaseSelection(WordCase::Downcase),
        ],
    );
    assert_eq!(text(&st), "heLLO");
    // `~` toggles.
    let st = run(
        "aBc",
        &[
            Command::EnterVisual {
                kind: SelectKind::Charwise,
            },
            Command::MoveRight,
            Command::MoveRight,
            Command::CaseSelection(WordCase::Toggle),
        ],
    );
    assert_eq!(text(&st), "AbC");
}

#[test]
fn visual_r_replaces_every_selected_char() {
    // Select "hel" (v + 2l), `rx` → "xxxlo".
    let st = run(
        "hello",
        &[
            Command::EnterVisual {
                kind: SelectKind::Charwise,
            },
            Command::MoveRight,
            Command::MoveRight,
            Command::ReplaceSelectionChar('x'),
        ],
    );
    assert_eq!(text(&st), "xxxlo");
    assert_eq!(st.mode(), Mode::Normal);
    assert_eq!(st.cursor().0, 0);
}

#[test]
fn visual_r_keeps_newlines_across_a_linewise_selection() {
    // Linewise-select two lines, `r-` fills every char with '-' but keeps the line break.
    let st = run(
        "ab\ncd\n",
        &[
            Command::EnterVisual {
                kind: SelectKind::Linewise,
            },
            Command::Move(1, Motion::Down),
            Command::ReplaceSelectionChar('-'),
        ],
    );
    assert_eq!(text(&st), "--\n--\n");
}

#[test]
fn visual_p_replaces_selection_and_swaps_the_register() {
    // Yank "foo" (chars via y3l), select "bar" (v + 2l), then `p` replaces it with "foo".
    let st = run(
        "foo bar",
        &[
            Command::Yank(3, Motion::Right), // unnamed = "foo"
            Command::Move(4, Motion::Right), // onto 'b'
            Command::EnterVisual {
                kind: SelectKind::Charwise,
            },
            Command::MoveRight,
            Command::MoveRight, // select "bar"
            Command::PasteSelection {
                swap: true,
                count: 1,
            },
        ],
    );
    assert_eq!(text(&st), "foo foo");
    assert_eq!(st.mode(), Mode::Normal);
    // The swap put the replaced "bar" into the unnamed register — a following `p` pastes it.
    assert_eq!(st.register().text(), b"bar");
}

#[test]
fn visual_capital_p_preserves_the_register() {
    // `P` replaces but does NOT clobber the register, so the same text can overwrite again.
    let st = run(
        "foo bar",
        &[
            Command::Yank(3, Motion::Right),
            Command::Move(4, Motion::Right),
            Command::EnterVisual {
                kind: SelectKind::Charwise,
            },
            Command::MoveRight,
            Command::MoveRight,
            Command::PasteSelection {
                swap: false,
                count: 1,
            },
        ],
    );
    assert_eq!(text(&st), "foo foo");
    assert_eq!(st.register().text(), b"foo", "P keeps the register intact");
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
    assert_eq!(st.selection_span(), Some((BytePos(0), BytePos(1))));
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
    assert_eq!(
        st.selection_span(),
        Some((BytePos(0), BytePos(3))),
        "v + l + l selects 'hel'"
    );
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
        assert_eq!(
            st.block_spans(),
            Some(vec![(BytePos(0), BytePos(2)), (BytePos(4), BytePos(6))])
        );
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
        assert_eq!(st.cursor().0, 0, "cursor at the block's top-left");
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
        assert_eq!(st.cursor().0, 0);
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
        // Geometry directly on explicit corners. Cols [1,3] over three rows; the middle line "x" ends at
        // col 1, so it contributes an empty range at its end.
        let b = b"abcd\nx\nefgh";
        let block = block_geom(b, 3, 8, 4, false); // (row0,col3) .. (row2,col1)
        assert_eq!((block.lo, block.hi), (1, 3));
        assert_eq!(block.spans(), vec![(1, 4), (6, 6), (8, 11)]);
        assert_eq!(block.yank_text(b), b"bcd\n\nfgh");
    }

    #[test]
    fn block_geometry_counts_screen_cells() {
        // `a<Tab>bc` at ts=4: the tab covers cells 1-3. A corner on the tab takes all of it, so the block
        // over `abcdefgh` below is cells 1-3 (nvim v0.12.6 `jll<C-v>ky`: `\t` / `bcd`).
        let b = b"a\tbc\nabcdefgh";
        let block = block_geom(b, 1, 7, 4, false);
        assert_eq!((block.lo, block.hi, block.width()), (1, 3, 3));
        assert_eq!(block.yank_text(b), b"\t\nbcd");
        // A block edge inside the tab yanks its inside cells as spaces (`<C-v>jfby`: `a ` / `ab`).
        let block = block_geom(b, 0, 6, 4, false);
        assert_eq!((block.lo, block.hi), (0, 1));
        assert_eq!(block.yank_text(b), b"a \nab");
        let top = block.rows[0];
        assert_eq!((top.ds, top.de, top.in_r, top.out_r), (0, 2, 1, 2));
        // A wide char is two cells: `a宽bc` over `abcdefgh`, corners on `宽` and `c` → cells 1-2.
        let b = "a宽bc\nabcdefgh".as_bytes();
        let block = block_geom(b, 1, 9, 4, false);
        assert_eq!((block.lo, block.hi), (1, 2));
        assert_eq!(block.yank_text(b), "宽\nbc".as_bytes());
        // A line that ends before the block's left edge yanks as the block's width in spaces (Vim
        // `block_prep`: `is_short` with `endspaces = width`).
        let b = b"abcdef\na\nabcdef";
        let block = block_geom(b, 2, 12, 4, false);
        assert_eq!(block.yank_text(b), b"cd\n  \ncd");
    }

    /// #1251: the virtual columns are the cells the renderer paints, as nvim v0.12.6 counts them
    /// (`virtcol()` on each byte): `<ff>` 4, `^A` 2, `<85>` 4, and a mark after `<80>` composes onto it.
    #[test]
    fn vcol_helpers_count_the_cells_nvim_draws() {
        let b = b"\xff\x01\xc2\x85a";
        assert_eq!(vcols_at(b, 0, 8), (0, 3));
        assert_eq!(vcols_at(b, 1, 8), (4, 5));
        assert_eq!(vcols_at(b, 2, 8), (6, 9));
        assert_eq!(vcols_at(b, 4, 8), (10, 10));
        assert_eq!(line_cells(b, 0, 8), 11);
        let b = b"a\x80\xcc\x81b";
        assert_eq!(
            vcols_at(b, 4, 8),
            (5, 5),
            "the mark after <80> takes no cell"
        );
        assert_eq!(line_cells(b, 0, 8), 6);
        assert_eq!(
            text_cells(b"\xcc\x81", 8),
            1,
            "a lone mark at a line start shows on a cell"
        );
        // A cut 3-byte char is one `<xx>` per byte.
        assert_eq!(line_cells(b"\xea\xb0a", 0, 8), 9);
    }

    #[test]
    fn vcol_helpers_round_trip_tabs_and_wide_chars() {
        let b = "\tx宽y".as_bytes();
        assert_eq!(vcols_at(b, 0, 4), (0, 3));
        assert_eq!(vcols_at(b, 1, 4), (4, 4));
        assert_eq!(vcols_at(b, 2, 4), (5, 6));
        assert_eq!(vcols_at(b, 5, 4), (7, 7));
        assert_eq!(
            vcols_at(b, b.len(), 4),
            (8, 8),
            "line end is the cell past the last char"
        );
        assert_eq!(at_vcol(b, 0, 2, 4), 0, "a cell inside the tab is the tab");
        assert_eq!(
            at_vcol(b, 0, 6, 4),
            2,
            "the second cell of the wide char is that char"
        );
        assert_eq!(
            at_vcol(b, 0, 99, 4),
            5,
            "past the end stops on the last char"
        );
        assert_eq!(line_cells(b, 0, 4), 8);
        assert_eq!(text_cells(b"a\tb", 4), 5);
    }

    /// `<C-v>j` from the start of `text`, then `cmds`.
    fn block_then(text: &str, cmds: &[Command]) -> EditorState {
        let mut all = vec![
            Command::EnterVisual {
                kind: SelectKind::Blockwise,
            },
            Command::MoveDown,
        ];
        all.extend_from_slice(cmds);
        run(text, &all)
    }

    const PASTE_AFTER: Command = Command::Paste {
        after: true,
        count: 1,
        move_after: false,
    };

    #[test]
    fn block_case_and_replace_act_on_the_block_only() {
        // #694 (nvim v0.12.6): `abc\ndef` `<C-v>jU` → `Abc\nDef`, `<C-v>jrx` → `xbc\nxef`.
        let up = block_then("abc\ndef", &[Command::CaseSelection(WordCase::Upcase)]);
        assert_eq!(text(&up), "Abc\nDef");
        assert_eq!(up.cursor().0, 0);
        let rot = block_then("abc\ndef", &[Command::CaseSelection(WordCase::Rot13)]);
        assert_eq!(text(&rot), "nbc\nqef");
        let r = block_then("abc\ndef", &[Command::ReplaceSelectionChar('x')]);
        assert_eq!(text(&r), "xbc\nxef");
        // A double-width replacement fills two cells; an odd cell left over becomes a space.
        let wide = run(
            "a宽bc\nabcdefgh",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Blockwise,
                },
                Command::MoveRight,
                Command::ReplaceSelectionChar('宽'),
            ],
        );
        assert_eq!(text(&wide), "宽 bc\nabcdefgh");
    }

    #[test]
    fn block_paste_past_a_final_newline_adds_no_blank_line() {
        // #695 (nvim v0.12.6, buffers with a final newline): `gg<C-v>jlyGp` and `<C-v>jyGp`.
        let st = run(
            "abcd\nefgh\nXY\nZW\n",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Blockwise,
                },
                Command::MoveDown,
                Command::MoveRight,
                Command::YankSelection,
                Command::Move(4, Motion::GotoLine),
                PASTE_AFTER,
            ],
        );
        assert_eq!(text(&st), "abcd\nefgh\nXY\nZabW\n ef\n");
        let st = block_then(
            "ab\ncd\n\n",
            &[
                Command::YankSelection,
                Command::Move(3, Motion::GotoLine),
                PASTE_AFTER,
            ],
        );
        assert_eq!(text(&st), "ab\ncd\na\nc\n");
        assert_eq!(st.cursor().0, 6, "top-left of the pasted block");
    }

    #[test]
    fn block_paste_into_the_empty_last_line_starts_there() {
        // #1080: `Go<Esc>` then `p` of a block lands in the new empty line, with or without a final
        // newline before it (nvim v0.12.6: `ab\ncd` + `a\nc` → `ab\ncd\na\nc`).
        // `o` ends the unterminated `ab\ncd`; the lines are then line-terminated (RFC-0024).
        for (start, want) in [("ab\ncd", "ab\ncd\na\nc\n"), ("ab\ncd\n", "ab\ncd\na\nc\n")] {
            let st = block_then(
                start,
                &[
                    Command::YankSelection,
                    Command::Move(2, Motion::GotoLine),
                    Command::OpenBelow,
                    Command::EnterNormal,
                    PASTE_AFTER,
                ],
            );
            assert_eq!(text(&st), want, "from {start:?}");
            assert_eq!(st.cursor().0, 6);
        }
        // An empty buffer: the first row fills its one line.
        let mut st = run("ab\ncd", &[]);
        apply_command(
            &mut st,
            &Command::EnterVisual {
                kind: SelectKind::Blockwise,
            },
        );
        apply_command(&mut st, &Command::MoveDown);
        apply_command(&mut st, &Command::YankSelection);
        let reg = st.view.registers.get(None).clone();
        let mut empty = EditorState::new(Vec::new());
        empty.view.registers.yank(None, reg);
        apply_command(&mut empty, &PASTE_AFTER);
        assert_eq!(text(&empty), "a\nc\n");
    }

    #[test]
    fn visual_put_shapes_by_selection_and_register_kind() {
        // #704 (nvim v0.12.6). Linewise selection + charwise register: the text becomes its own line.
        let st = run(
            "a\nb\nc",
            &[
                Command::Yank(1, Motion::InnerWord),
                Command::MoveDown,
                Command::EnterVisual {
                    kind: SelectKind::Linewise,
                },
                Command::PasteSelection {
                    swap: true,
                    count: 1,
                },
            ],
        );
        assert_eq!(text(&st), "a\na\nc");
        assert_eq!(st.cursor().0, 2);
        assert_eq!(st.view.registers.get(None).text(), b"b\n");
        // Charwise selection + linewise register: the line splits around the put lines.
        let st = run(
            "foo bar\nx",
            &[
                Command::MoveDown,
                Command::Yank(1, Motion::Line),
                Command::MoveUp,
                Command::EnterVisual {
                    kind: SelectKind::Charwise,
                },
                Command::Move(1, Motion::InnerWord),
                Command::PasteSelection {
                    swap: false,
                    count: 1,
                },
            ],
        );
        assert_eq!(text(&st), "\nx\n bar\nx");
        assert_eq!(st.cursor().0, 1);
        assert_eq!(
            st.view.registers.get(None).text(),
            b"x\n",
            "`P` keeps the register"
        );
        // Block selection + one-line charwise register: the text goes on every row.
        let st = run(
            "abc\ndef\nghi",
            &[
                Command::Yank(1, Motion::Right),
                Command::EnterVisual {
                    kind: SelectKind::Blockwise,
                },
                Command::MoveDown,
                Command::MoveRight,
                Command::PasteSelection {
                    swap: true,
                    count: 1,
                },
            ],
        );
        assert_eq!(text(&st), "ac\naf\nghi");
        assert_eq!(st.view.registers.get(None).text(), b"ab\nde");
        assert!(st.view.registers.get(None).is_blockwise());
        // One undo restores the selection's text.
        let mut st = st;
        apply_command(&mut st, &Command::Undo);
        assert_eq!(text(&st), "abc\ndef\nghi");
    }

    /// The emptied buffer with `"a` holding charwise `ab` and the unnamed register linewise `ab\n` (as
    /// `"ayiwggdG` leaves them), in `kind` Visual mode on its one line.
    fn emptied_in_visual(kind: SelectKind) -> EditorState {
        let mut st = EditorState::new(Vec::new());
        st.view
            .registers
            .yank(Some('a'), crate::Register::charwise(b"ab".to_vec()));
        st.view
            .registers
            .delete(None, crate::Register::linewise(b"ab\n".to_vec()));
        apply_command(&mut st, &Command::EnterVisual { kind });
        st
    }

    const PUT_SWAP: Command = Command::PasteSelection {
        swap: true,
        count: 1,
    };

    #[test]
    fn visual_put_on_the_emptied_buffer_puts_and_keeps_the_registers() {
        // #1346 (nvim v0.12.6): `op_delete` deletes nothing in the emptied buffer (`ML_EMPTY`), so `v"ap`,
        // `V"ap` and `<C-v>"ap` put `ab` and the unnamed register keeps `ab\n` linewise.
        for kind in [
            SelectKind::Charwise,
            SelectKind::Linewise,
            SelectKind::Blockwise,
        ] {
            let mut st = emptied_in_visual(kind);
            apply_command(&mut st, &Command::SetRegister(Some('a')));
            apply_command(&mut st, &PUT_SWAP);
            assert_eq!(text(&st), "ab\n", "{kind:?}");
            let want_col = usize::from(kind != SelectKind::Linewise);
            assert_eq!(st.cursor().0, want_col, "{kind:?}");
            let unnamed = st.view.registers.get(None);
            assert_eq!(unnamed.text(), b"ab\n", "{kind:?}");
            assert!(unnamed.is_linewise(), "{kind:?}");
            // One undo takes the put and the emptied line back: the buffer is emptied again.
            apply_command(&mut st, &Command::Undo);
            assert_eq!(text(&st), "", "{kind:?}");
        }
        // `vp` / `vP` of the linewise unnamed register splits the line: nvim gives `["", "ab"]`.
        for swap in [true, false] {
            let mut st = emptied_in_visual(SelectKind::Charwise);
            apply_command(&mut st, &Command::PasteSelection { swap, count: 1 });
            assert_eq!(text(&st), "\nab\n", "swap {swap}");
            assert_eq!(st.cursor().0, 1, "swap {swap}");
            assert_eq!(st.view.registers.get(None).text(), b"ab\n");
        }
        // A count puts the text that many times: `v3"ap` gives `ababab`, the cursor on its last char.
        let mut st = emptied_in_visual(SelectKind::Charwise);
        apply_command(&mut st, &Command::SetRegister(Some('a')));
        apply_command(
            &mut st,
            &Command::PasteSelection {
                swap: true,
                count: 3,
            },
        );
        assert_eq!(text(&st), "ababab\n");
        assert_eq!(st.cursor().0, 5);
    }

    #[test]
    fn visual_put_on_an_empty_line_deletes_nothing_where_nvim_does() {
        // #1346 (nvim v0.12.6): a charwise selection on the empty LAST line has nothing to delete, so
        // `yiwjvp` on `ab\n\n` puts `ab` there and the unnamed register keeps `ab`.
        let yank_then = |initial: &str, kind: SelectKind, put: Command| {
            let mut st = run(initial, &[Command::Yank(1, Motion::InnerWord)]);
            for c in [Command::MoveDown, Command::EnterVisual { kind }, put] {
                apply_command(&mut st, &c);
            }
            st
        };
        let st = yank_then("ab\n\n", SelectKind::Charwise, PUT_SWAP);
        assert_eq!(text(&st), "ab\nab\n");
        assert_eq!(st.cursor().0, 4);
        assert_eq!(st.view.registers.get(None).text(), b"ab");
        // A linewise register splits the empty last line: `yyjvp` gives `["ab", "", "ab", ""]`.
        let mut st = run("ab\n\n", &[Command::Yank(1, Motion::Line)]);
        for c in [
            Command::MoveDown,
            Command::EnterVisual {
                kind: SelectKind::Charwise,
            },
            PUT_SWAP,
        ] {
            apply_command(&mut st, &c);
        }
        assert_eq!(text(&st), "ab\n\nab\n\n");
        assert_eq!(st.cursor().0, 4);
        // A one-row block on an empty line deletes nothing either, mid-buffer too: `yiwj<C-v>p` on
        // `ab\n\ncd\n` puts `ab` on that line and the unnamed register keeps `ab` charwise.
        let st = yank_then("ab\n\ncd\n", SelectKind::Blockwise, PUT_SWAP);
        assert_eq!(text(&st), "ab\nab\ncd\n");
        let unnamed = st.view.registers.get(None);
        assert_eq!(unnamed.text(), b"ab");
        assert!(!unnamed.is_linewise() && !unnamed.is_blockwise());
        // A charwise selection on an empty line that is not the last takes its newline (nvim joins).
        let st = yank_then("ab\n\ncd\n", SelectKind::Charwise, PUT_SWAP);
        assert_eq!(text(&st), "ab\nabcd\n");
        assert_eq!(st.view.registers.get(None).text(), b"\n");
    }

    #[test]
    fn visual_repeat_measures_screen_columns() {
        // #1064 (nvim v0.12.6, ts=4): `vld` over `<Tab>a` is five cells, so `.` on a plain line deletes five.
        let st = run(
            "\tabcdef\nabcdefgh",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Charwise,
                },
                Command::MoveRight,
                Command::DeleteSelection,
                Command::MoveDown,
                Command::MoveLineStart,
                Command::DeleteSelection, // `.` replays the recorded selection operator
            ],
        );
        assert_eq!(text(&st), "bcdef\nfgh");
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
        assert_eq!(st.cursor().0, 0, "cursor returns to the block top-left");
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
    fn block_append_dollar_is_ragged_to_each_line_end() {
        // `` <C-v>jj$A `` appends at EACH line's own end (ragged), not a fixed column. `$` sets curswant to
        // MAXCOL, which the block-append detects. Verified vs nvim v0.12.4 (fixture block_append_ragged).
        let st = run(
            "a\nabc\nab",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Blockwise,
                },
                Command::MoveDown,
                Command::MoveDown,
                Command::Move(1, Motion::LineEnd), // `$` → ragged block, curswant = MAXCOL
                Command::BlockInsert(BlockInsertKind::Append),
                Command::InsertChar('X'),
                Command::EnterNormal,
            ],
        );
        assert_eq!(
            text(&st),
            "aX\nabcX\nabX",
            "X appended at each line's own end"
        );
    }

    #[test]
    fn block_insert_replay_rebuilds_the_block_at_the_cursor() {
        // Dot-repeat drives a bare `BlockInsert` (no live selection) — the planner rebuilds the block from
        // the retained geometry at the caret. This asserts the CORE side of the replay: a first block `I`
        // stores height 2, then a second bare `BlockInsert` at a moved cursor re-applies over 2 rows.
        // Expected bytes match nvim v0.12.4 (`<C-v>jIX<Esc>jj.`).
        let st = run(
            "aaaa\nbbbb\ncccc\ndddd",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Blockwise,
                },
                Command::MoveDown, // 2-row block at col 0
                Command::BlockInsert(BlockInsertKind::Insert),
                Command::InsertChar('X'),
                Command::EnterNormal,
                // Simulate `jj.`: move down two rows, then the recorded replay (bare BlockInsert + text).
                Command::MoveDown,
                Command::MoveDown,
                Command::BlockInsert(BlockInsertKind::Insert),
                Command::InsertChar('X'),
                Command::EnterNormal,
            ],
        );
        assert_eq!(text(&st), "Xaaaa\nXbbbb\nXcccc\nXdddd");
        assert_eq!(
            st.cursor().0,
            12,
            "cursor rests at the rebuilt block's top-left"
        );
    }

    #[test]
    fn block_change_replay_preserves_width() {
        // A bare `BlockInsert(Change)` replay deletes the stored WIDTH per row at the caret then inserts.
        // Matches nvim `<C-v>jlcXY<Esc>jj.`.
        let st = run(
            "aaaa\nbbbb\ncccc\ndddd",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Blockwise,
                },
                Command::MoveRight,
                Command::MoveDown, // width-2, 2-row block
                Command::BlockInsert(BlockInsertKind::Change),
                Command::InsertChar('X'),
                Command::InsertChar('Y'),
                Command::EnterNormal, // -> "XYaa\nXYbb\ncccc\ndddd", caret col 1
                Command::MoveDown,
                Command::MoveDown, // caret row 2 col 1
                Command::BlockInsert(BlockInsertKind::Change),
                Command::InsertChar('X'),
                Command::InsertChar('Y'),
                Command::EnterNormal,
            ],
        );
        assert_eq!(text(&st), "XYaa\nXYbb\ncXYc\ndXYd");
    }

    #[test]
    fn block_insert_replay_with_no_prior_geometry_is_a_plain_insert() {
        // A bare `BlockInsert` with neither a selection nor retained geometry degrades to a plain single-row
        // Insert (the historical early-out) — it must not panic or replicate.
        let st = run(
            "abc\ndef",
            &[
                Command::BlockInsert(BlockInsertKind::Insert),
                Command::InsertChar('X'),
                Command::EnterNormal,
            ],
        );
        assert_eq!(
            text(&st),
            "Xabc\ndef",
            "no geometry -> plain insert on one row"
        );
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

    // `c<C-v>{motion}` — operator-forced blockwise CHANGE — deletes the block column then replicates the
    // typed text down every row, exactly like Visual-block `c`. Bytes below are captured from nvim v0.12.4
    // (`c<C-v>2jX<Esc>` on abc/def/ghi → Xbc/Xef/Xhi; deleted block "a\nd\ng" blockwise).
    fn forced_change_blockwise(count: u32) -> Command {
        Command::OpForced {
            op: OpKind::Change,
            count,
            motion: Motion::Down,
            wise: ForcedWise::Blockwise,
        }
    }

    #[test]
    fn forced_blockwise_change_replicates_typed_text_down_all_rows() {
        let st = run(
            "abc\ndef\nghi",
            &[
                forced_change_blockwise(2), // block col 0, rows 0..2
                Command::InsertChar('X'),
                Command::EnterNormal,
            ],
        );
        assert_eq!(text(&st), "Xbc\nXef\nXhi");
        assert_eq!(st.mode(), Mode::Normal);
        assert_eq!(st.cursor().0, 0, "cursor returns to the block top-left");
        assert!(
            st.register().is_blockwise(),
            "the deleted block is captured blockwise"
        );
        assert_eq!(st.register().text(), b"a\nd\ng");
    }

    #[test]
    fn forced_blockwise_change_replicates_over_a_short_row() {
        // nvim: `c<C-v>2jX<Esc>` on abc/d/ghi → Xbc/X/Xhi (col 0 exists on every row, so all replicate).
        let st = run(
            "abc\nd\nghi",
            &[
                forced_change_blockwise(2),
                Command::InsertChar('X'),
                Command::EnterNormal,
            ],
        );
        assert_eq!(text(&st), "Xbc\nX\nXhi");
        assert_eq!(st.register().text(), b"a\nd\ng");
    }

    #[test]
    fn forced_blockwise_change_replicates_multichar_text() {
        // nvim: `c<C-v>2jXY<Esc>` on abc/def/ghi → XYbc/XYef/XYhi, cursor at [1,1] — a block CHANGE rests
        // one char left of the typed text (normal Insert-exit), NOT snapped to the top-left like `I`/`A`.
        let st = run(
            "abc\ndef\nghi",
            &[
                forced_change_blockwise(2),
                Command::InsertChar('X'),
                Command::InsertChar('Y'),
                Command::EnterNormal,
            ],
        );
        assert_eq!(text(&st), "XYbc\nXYef\nXYhi");
        assert_eq!(
            st.cursor().0,
            1,
            "block change rests one char left of the typed run"
        );
    }

    #[test]
    fn visual_block_change_multichar_cursor_rests_left_of_typed_text() {
        // The SAME cursor rule via the Visual-block `c` path (shared `block_replicate`): nvim leaves the
        // caret at [1,1] after `<C-v>2jcXY<Esc>` on abc/def/ghi, while `I`/`A` snap back to col 0.
        let st = run(
            "abc\ndef\nghi",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Blockwise,
                },
                Command::MoveDown,
                Command::MoveDown,
                Command::BlockInsert(BlockInsertKind::Change),
                Command::InsertChar('X'),
                Command::InsertChar('Y'),
                Command::EnterNormal,
            ],
        );
        assert_eq!(text(&st), "XYbc\nXYef\nXYhi");
        assert_eq!(
            st.cursor().0,
            1,
            "Visual-block change also rests left of the typed run"
        );
    }

    #[test]
    fn forced_blockwise_change_over_a_wide_single_row() {
        // nvim: `c<C-v>2lX<Esc>` on abcd/efgh → Xd/efgh (cols 0..2 on the top row only; register "abc").
        let st = run(
            "abcd\nefgh",
            &[
                Command::OpForced {
                    op: OpKind::Change,
                    count: 2,
                    motion: Motion::Right,
                    wise: ForcedWise::Blockwise,
                },
                Command::InsertChar('X'),
                Command::EnterNormal,
            ],
        );
        assert_eq!(text(&st), "Xd\nefgh");
        assert!(st.register().is_blockwise());
        assert_eq!(st.register().text(), b"abc");
    }

    #[test]
    fn forced_blockwise_change_is_a_single_undo_unit() {
        // nvim: the whole block-delete + replicate is ONE change; a single `u` restores the buffer.
        let st = run(
            "abc\ndef\nghi",
            &[
                forced_change_blockwise(2),
                Command::InsertChar('X'),
                Command::EnterNormal,
                Command::Undo,
            ],
        );
        assert_eq!(
            text(&st),
            "abc\ndef\nghi",
            "a single undo reverts the delete AND the replicate"
        );
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
            Some((BytePos(0), BytePos(3))),
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
            Some((BytePos(0), BytePos(2))),
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
            st.cursor().0,
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
        // A bare motion in Select moves the cursor and keeps the anchor — exactly as in Visual. (`gh` is
        // `EnterVisual` then `EnterSelect`; `EnterSelect` alone only toggles an existing selection.)
        let st = run(
            "hello",
            &[
                Command::EnterVisual {
                    kind: SelectKind::Charwise,
                },
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
        assert_eq!(st.selection_span(), Some((BytePos(0), BytePos(3))));
    }

    #[test]
    fn enter_select_outside_a_selection_is_inert() {
        // The `v_CTRL-O` return appends `EnterSelect` after the borrowed command; when that command ended
        // the selection (`y`, `<Esc>`), Select must not start a fresh one at the cursor.
        let st = run(
            "hello",
            &[Command::EnterSelect {
                kind: SelectKind::Charwise,
            }],
        );
        assert_eq!(st.mode(), Mode::Normal);
        assert_eq!(st.selection_span(), None);
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

/// #606 / #792: Visual line operators ([`Command::VisualOp`]) and Select replace, against nvim v0.12.6
/// (the `v_*`/`vb_*`/`s_*` oracle fixtures carry the same cases through the real key path).
#[cfg(test)]
mod visual_op_tests {
    use crate::editor::*;
    use crate::VisualOp;

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

    fn visual(kind: SelectKind) -> Command {
        Command::EnterVisual { kind }
    }

    const CHAR: SelectKind = SelectKind::Charwise;
    const BLOCK: SelectKind = SelectKind::Blockwise;

    #[test]
    fn lines_delete_is_linewise_and_keeps_the_column() {
        // `lvjD` on abcd/efgh/ijkl: both lines go, the cursor keeps column 1 (nvim `nostartofline`).
        let st = run(
            "abcd\nefgh\nijkl",
            &[
                Command::MoveRight,
                visual(CHAR),
                Command::MoveDown,
                Command::VisualOp(VisualOp::Lines(OpKind::Delete)),
            ],
        );
        assert_eq!(text(&st), "ijkl");
        assert_eq!(st.cursor().0, 1);
        assert_eq!(st.mode(), Mode::Normal);
        assert!(st.register().is_linewise());
        assert_eq!(st.register().text(), b"abcd\nefgh\n");
    }

    #[test]
    fn lines_delete_through_the_end_takes_the_newline_before() {
        // `jvj$D`: no blank line is left; the cursor rides `$` onto the new last line's end.
        let st = run(
            "abcd\nefgh\nijkl",
            &[
                Command::MoveDown,
                visual(CHAR),
                Command::MoveDown,
                Command::Move(1, Motion::LineEnd),
                Command::VisualOp(VisualOp::Lines(OpKind::Delete)),
            ],
        );
        assert_eq!(text(&st), "abcd");
        assert_eq!(st.cursor().0, 3);
        assert_eq!(st.register().text(), b"efgh\nijkl\n");
    }

    #[test]
    fn block_to_eol_delete_runs_to_each_row_end() {
        // `l<C-v>jjD` on abcd/e/ijkl → a/e/i, register `bcd\n\njkl` blockwise.
        let st = run(
            "abcd\ne\nijkl",
            &[
                Command::MoveRight,
                visual(BLOCK),
                Command::MoveDown,
                Command::MoveDown,
                Command::VisualOp(VisualOp::BlockToEol(OpKind::Delete)),
            ],
        );
        assert_eq!(text(&st), "a\ne\ni");
        assert_eq!(st.register().text(), b"bcd\n\njkl");
        assert!(st.register().is_blockwise());
    }

    #[test]
    fn shift_moves_the_selected_lines_count_levels() {
        // `vj2>` with shiftwidth 4: two lines, eight columns; cursor on the first line at the wanted column
        // (0, 'nostartofline', #1176).
        let st = run(
            "a\nb\nc",
            &[
                visual(CHAR),
                Command::MoveDown,
                Command::VisualOp(VisualOp::Shift {
                    left: false,
                    count: 2,
                }),
            ],
        );
        assert_eq!(text(&st), "        a\n        b\nc");
        assert_eq!(st.cursor().0, 0);
        assert_eq!(st.mode(), Mode::Normal);
        let st = run(
            "            a\nb",
            &[
                visual(CHAR),
                Command::VisualOp(VisualOp::Shift {
                    left: true,
                    count: 2,
                }),
            ],
        );
        assert_eq!(text(&st), "    a\nb");
    }

    /// Visual-block `>`/`<` from `cursor` over the next `down` lines, `count` levels, at ts=8 sw=4 (#1180).
    fn block_shift(
        initial: &str,
        cursor: usize,
        down: usize,
        left: bool,
        count: u32,
        et: bool,
    ) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        st.view.indent.tab_width = 8;
        st.view.indent.style = if et {
            crate::editor::IndentStyle::Space
        } else {
            crate::editor::IndentStyle::Tab
        };
        st.set_cursor(BytePos(cursor));
        apply_command(&mut st, &visual(BLOCK));
        for _ in 0..down {
            apply_command(&mut st, &Command::MoveDown);
        }
        apply_command(&mut st, &Command::VisualOp(VisualOp::Shift { left, count }));
        st
    }

    /// #1180: the nvim v0.12.6 bytes of `:help v_b_>` / `v_b_<` (the `vb_shift_*` oracle fixtures).
    #[test]
    fn block_shift_works_from_the_left_column() {
        // `w<C-v>j>`: from column 4, the lines before it stay.
        let st = block_shift("abc def\nghi jkl\nmno", 4, 1, false, 1, true);
        assert_eq!(text(&st), "abc     def\nghi     jkl\nmno");
        assert_eq!(st.cursor().0, 4);
        assert_eq!(st.mode(), Mode::Normal);
        // The blanks just left of the block join the shifted run: tabs from where it starts under noet.
        let st = block_shift("ab  cd\nef  gh", 4, 1, false, 1, false);
        assert_eq!(text(&st), "ab\tcd\nef\tgh");
        // On a tab: its width counts, then the count's levels.
        let st = block_shift("a\tb\tc\nd\te\tf", 1, 1, false, 1, true);
        assert_eq!(text(&st), "a           b\tc\nd           e\tf");
        // A row the edge cuts a tab on (neither corner sits on it) shifts from the tab's start.
        let st = block_shift("abcdefgh\na\tb\nabcdefgh", 3, 2, false, 1, false);
        assert_eq!(text(&st), "abc    defgh\na\t    b\nabc    defgh");
        // A cut wide character: the shift goes in before it.
        let st = block_shift("abcdefgh\na中中b\nabcdefgh", 2, 2, false, 1, true);
        assert_eq!(text(&st), "ab    cdefgh\na    中中b\nab    cdefgh");
        // A short row and an empty one are left alone.
        let st = block_shift("abcdef\nab\n\nabcdef", 4, 3, false, 1, true);
        assert_eq!(text(&st), "abcd    ef\nab\n\nabcd    ef");
        // `<` removes blanks from the edge only, up to the next text; a count; a tab it splits.
        let st = block_shift("ab      cd\nef      gh", 3, 1, true, 1, true);
        assert_eq!(text(&st), "ab  cd\nef  gh");
        let st = block_shift("ab    cd\nef    gh", 1, 1, true, 1, true);
        assert_eq!(text(&st), "ab    cd\nef    gh");
        let st = block_shift("ab\t\tcd\nef\t\tgh", 2, 1, true, 1, true);
        assert_eq!(text(&st), "ab\t    cd\nef\t    gh");
        let st = block_shift("abcdefghijklmnop\na\t\tx", 10, 1, true, 3, true);
        assert_eq!(text(&st), "abcdefghijklmnop\na\tx");
        assert_eq!(st.cursor().0, 8);
    }

    /// #1142: a block change broken by `<CR>` copies what the top row grew by into the lines below it by
    /// number (the new line first), as nvim's `op_change`; `I` that leaves the top row copies nothing.
    #[test]
    fn block_change_with_a_newline_copies_the_top_row_growth() {
        let typed = |op: Command, keys: &str| {
            let mut cmds = vec![Command::MoveRight, visual(BLOCK), Command::MoveDown, op];
            for c in keys.chars() {
                cmds.push(match c {
                    '\n' => Command::InsertNewline,
                    c => Command::InsertChar(c),
                });
            }
            cmds.push(Command::EnterNormal);
            cmds
        };
        // `l<C-v>jCx<CR>y<Esc>`: the `x` goes into the `y` line; `ijkl` is not a block row any more.
        let st = run(
            "abcd\nefgh\nijkl",
            &typed(
                Command::VisualOp(VisualOp::BlockToEol(OpKind::Change)),
                "x\ny",
            ),
        );
        assert_eq!(text(&st), "ax\nyx\ne\nijkl");
        assert_eq!(st.cursor().0, 3, "on the `y`, where <Esc> left it");
        // `c` over two columns: the top row did not grow (its tail moved down), so nothing is copied.
        let mut cmds = typed(Command::ChangeSelection, "x\ny");
        cmds.insert(3, Command::MoveRight);
        let st = run("abcd\nefgh\nijkl", &cmds);
        assert_eq!(text(&st), "ax\nyd\neh\nijkl");
        // `I`: the insert left the top row, so nothing is copied.
        let st = run(
            "abcd\nefgh\nijkl",
            &typed(Command::BlockInsert(BlockInsertKind::Insert), "x\ny"),
        );
        assert_eq!(text(&st), "ax\nybcd\nefgh\nijkl");
    }

    #[test]
    fn join_covers_the_selected_lines_and_a_single_line_joins_the_next() {
        let st = run(
            "a\nb\nc\nd",
            &[
                visual(SelectKind::Linewise),
                Command::MoveDown,
                Command::MoveDown,
                Command::VisualOp(VisualOp::Join { spaces: true }),
            ],
        );
        assert_eq!(text(&st), "a b c\nd");
        assert_eq!(st.cursor().0, 3);
        let st = run(
            "a\n  b\nc",
            &[
                visual(CHAR),
                Command::VisualOp(VisualOp::Join { spaces: false }),
            ],
        );
        assert_eq!(text(&st), "a  b\nc");
    }

    #[test]
    fn other_corner_stays_on_the_row_in_a_block() {
        // `l<C-v>jlO`: cursor (row 1, col 2) → (row 1, col 1); anchor (row 0, col 1) → (row 0, col 2).
        let st = run(
            "abcd\nefgh",
            &[
                Command::MoveRight,
                visual(BLOCK),
                Command::MoveDown,
                Command::MoveRight,
                Command::VisualOp(VisualOp::OtherCorner),
            ],
        );
        assert_eq!(st.cursor().0, 6);
        assert_eq!(st.view.anchor, Some(2));
    }

    #[test]
    fn select_replace_of_whole_lines_is_charwise_in_the_register() {
        // `gHX`: the line and its newline go, `X` starts the next line; nvim stores `abc\n` charwise.
        let st = run(
            "abc\ndef",
            &[
                visual(SelectKind::Linewise),
                Command::EnterSelect {
                    kind: SelectKind::Linewise,
                },
                Command::ReplaceSelection('X'),
                Command::EnterNormal,
            ],
        );
        assert_eq!(text(&st), "Xdef");
        assert_eq!(st.register().text(), b"abc\n");
        assert!(!st.register().is_linewise());
    }

    #[test]
    fn select_replace_of_a_block_replicates_the_typed_run() {
        // `g<C-h>` + extend + `XY<Esc>`: a block change whose first char is the key.
        let st = run(
            "abc\ndef",
            &[
                visual(BLOCK),
                Command::EnterSelect { kind: BLOCK },
                Command::MoveDown,
                Command::MoveRight,
                Command::ReplaceSelection('X'),
                Command::InsertChar('Y'),
                Command::EnterNormal,
            ],
        );
        assert_eq!(text(&st), "XYc\nXYf");
        assert!(st.register().is_blockwise());
        assert_eq!(st.register().text(), b"ab\nde");
    }
}
