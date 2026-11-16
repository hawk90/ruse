//! Insert-mode entry and Virtual Replace. Split from `editor/tests.rs` (#1397).

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
    fn undo_puts_the_cursor_where_the_change_began() {
        // #1063: not at the old byte offset. `x` on line 2, back to line 1, `u`: line 2 again.
        let st = run(
            "abc\ndef",
            &[
                Command::MoveDown,
                Command::MoveRight,
                Command::DeleteUnder(1),
                Command::MoveUp,
                Command::Undo,
            ],
        );
        assert_eq!((text(&st).as_str(), st.cursor().0), ("abc\ndef", 5));
        // Redo goes there too, and so do `g-` / `g+`.
        let st = run(
            "abc\ndef",
            &[
                Command::MoveDown,
                Command::MoveRight,
                Command::DeleteUnder(1),
                Command::Undo,
                Command::MoveUp,
                Command::Redo,
            ],
        );
        assert_eq!((text(&st).as_str(), st.cursor().0), ("abc\ndf", 5));
        let st = run(
            "abc\ndef",
            &[
                Command::MoveDown,
                Command::DeleteUnder(1),
                Command::MoveUp,
                Command::UndoOlder,
            ],
        );
        assert_eq!((text(&st).as_str(), st.cursor().0), ("abc\ndef", 4));
        let st = run(
            "abc\ndef",
            &[
                Command::MoveDown,
                Command::DeleteUnder(1),
                Command::UndoOlder,
                Command::MoveUp,
                Command::UndoNewer,
            ],
        );
        assert_eq!((text(&st).as_str(), st.cursor().0), ("abc\nef", 4));
    }

    #[test]
    fn undo_from_insert_may_leave_the_cursor_past_the_line_end() {
        // `Ax<C-o>u`: the append began past the last char, and Insert may rest there (Normal clamps).
        let st = run(
            "ab",
            &[
                Command::AppendLineEnd,
                Command::InsertChar('x'),
                Command::Undo,
            ],
        );
        assert_eq!((text(&st).as_str(), st.cursor().0), ("ab", 2));
        let st = run(
            "ab",
            &[
                Command::AppendLineEnd,
                Command::InsertChar('x'),
                Command::EnterNormal,
                Command::Undo,
            ],
        );
        assert_eq!((text(&st).as_str(), st.cursor().0), ("ab", 1));
    }

    #[test]
    fn open_below_on_last_line() {
        let st = run("ab", &[Command::OpenBelow, Command::InsertChar('X')]);
        assert_eq!(text(&st), "ab\nX\n");
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
                (crate::BytePos(0), crate::BytePos(5), "Hi".to_string()), // "hello" → "Hi"
                (crate::BytePos(6), crate::BytePos(11), "there".to_string()), // "world" → "there"
            ],
            crate::TransactionOrigin::Lsp,
        );
        assert_eq!(text(&st), "Hi there");
        // The whole set is one undo group (LSP origin).
        apply_command(&mut st, &Command::Undo);
        assert_eq!(text(&st), "hello world");
    }

    #[test]
    fn a_one_shot_jump_leaves_from_where_the_autoindent_went() {
        // #1368 (nvim v0.12.6: `O<C-o>G` on "\t" puts the jump on line 1 col 0): the jump off an opened,
        // untouched autoindent line records the start of the removed blanks, inside the text.
        let mut st = EditorState::new(b"\t\n".to_vec());
        st.view.indent.autoindent = true;
        apply_command(&mut st, &Command::OpenAbove);
        apply_command(&mut st, &Command::Move(1, Motion::LastLine));
        st.assert_invariants();
        assert_eq!(text(&st), "\n\t\n");
        assert_eq!(st.view.jumps.last(), Some(&0));
    }

    #[test]
    fn through_change_maps_around_one_changed_span() {
        assert_eq!(through_change(b"ab  \ncd", b"ab\ncd", 4), 2);
        assert_eq!(through_change(b"ab  \ncd", b"ab\ncd", 1), 1);
        assert_eq!(through_change(b"ab  \ncd", b"ab\ncd", 6), 4);
    }

    #[test]
    fn gi_on_the_emptied_buffer_keeps_its_offsets_inside_the_text() {
        // #1364: `gi` changes no text, so the emptied line it ran on is taken back, with what it stored.
        let mut st = EditorState::new(Vec::new());
        for cmd in [
            Command::EnterInsert,
            Command::EnterNormal,
            Command::InsertAtLastInsert,
        ] {
            apply_command(&mut st, &cmd);
            st.assert_invariants();
        }
        assert_eq!(text(&st), "");
    }

    #[test]
    fn apply_edits_on_the_emptied_buffer_end_with_its_newline() {
        // RFC-0024 §5: the edits address the empty text; what they give it is line-terminated.
        for (ins, want) in [("abc", "abc\n"), ("abc\n", "abc\n"), ("a\nb", "a\nb\n")] {
            let mut st = EditorState::new(Vec::new());
            assert!(st.apply_edits(
                &[(crate::BytePos(0), crate::BytePos(0), ins.to_string())],
                crate::TransactionOrigin::Lsp,
            ));
            assert_eq!(text(&st), want, "{ins:?}");
        }
        // The cursor stays before the added newline: a completion accepted in Insert mode types on.
        let mut st = EditorState::new(Vec::new());
        apply_command(&mut st, &Command::EnterInsert);
        st.apply_edits(
            &[(crate::BytePos(0), crate::BytePos(0), "foo".to_string())],
            crate::TransactionOrigin::Lsp,
        );
        apply_command(&mut st, &Command::InsertChar('x'));
        assert_eq!(text(&st), "foox\n");
    }

    #[test]
    fn apply_edits_batches_are_separate_undo_units_regardless_of_origin() {
        // Each apply_edits call is its own undo group (GroupHint::BreakBefore), so a later batch never
        // coalesces into an earlier one — independent of the caller-supplied origin (#305). This is the
        // contract the LSP format/rename/code-action flows rely on: applying LSP edits leaves the user's
        // prior edits as a distinct, still-undoable unit.
        let mut st = EditorState::new(b"abc".to_vec());
        st.apply_edits(
            &[(crate::BytePos(3), crate::BytePos(3), "X".to_string())],
            crate::TransactionOrigin::UserInput,
        ); // "abcX"
        st.apply_edits(
            &[(crate::BytePos(0), crate::BytePos(0), "Y".to_string())],
            crate::TransactionOrigin::Lsp,
        ); // "YabcX"
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
        st.apply_edits(
            &[
                (crate::BytePos(0), crate::BytePos(3), "X".to_string()),
                (crate::BytePos(2), crate::BytePos(5), "Y".to_string()),
            ],
            lsp,
        ); // overlap → refused
        assert_eq!(text(&st), "abcdef");
        st.apply_edits(
            &[(crate::BytePos(0), crate::BytePos(99), "Z".to_string())],
            lsp,
        ); // out of range → skipped
        assert_eq!(text(&st), "abcdef");
    }

    #[test]
    fn apply_edits_is_atomic_and_carries_the_cursor() {
        // #716: one bad edit refuses the whole batch, a split UTF-8 character is refused, and the cursor
        // moves with the text an edit above it shifts.
        let lsp = crate::TransactionOrigin::Lsp;
        let mut st = EditorState::new(b"abcdef".to_vec());
        assert!(!st.apply_edits(
            &[
                (crate::BytePos(0), crate::BytePos(1), "X".to_string()),
                (crate::BytePos(1000), crate::BytePos(1001), "Y".to_string())
            ],
            lsp
        ));
        assert_eq!(text(&st), "abcdef");
        let mut st = EditorState::new("한글".as_bytes().to_vec());
        assert!(!st.apply_edits(
            &[(crate::BytePos(1), crate::BytePos(2), "X".to_string())],
            lsp
        ));
        assert_eq!(text(&st), "한글");
        let mut st = EditorState::new(b"one\ntwo\n".to_vec());
        st.set_cursor(BytePos(5));
        assert!(st.apply_edits(
            &[(
                crate::BytePos(0),
                crate::BytePos(0),
                "AAAAAAA\n".to_string()
            )],
            lsp
        ));
        assert_eq!(st.cursor().0, 13);
        assert_eq!(&text(&st)[13..14], "w");
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
    fn autoindent_cleanup_removes_indent_on_esc_when_nothing_typed() {
        // `o<Esc>` with auto-indent and no text typed: the indent is removed (no trailing whitespace).
        let st = run(
            "ab\ncd",
            &[
                Command::OpenLineIndent {
                    kind: OpenKind::Below,
                    level: 1,
                },
                Command::EnterNormal,
            ],
        );
        assert_eq!(text(&st), "ab\n\ncd", "the auto-inserted '    ' is gone");
    }

    #[test]
    fn autoindent_cleanup_keeps_indent_when_content_typed() {
        // Typing a non-blank clears the pending flag, so the indent survives `<Esc>`.
        let st = run(
            "ab\ncd",
            &[
                Command::OpenLineIndent {
                    kind: OpenKind::Below,
                    level: 1,
                },
                Command::InsertChar('X'),
                Command::EnterNormal,
            ],
        );
        assert_eq!(text(&st), "ab\n    X\ncd", "typed content keeps its indent");
    }

    #[test]
    fn autoindent_cleanup_never_strips_a_pre_existing_blank_line() {
        // Entering/leaving Insert on an ALREADY-blank line must not delete it (the flag is only set by an
        // auto-indent open, never by plain `i`). Guards against data loss.
        let st = run("    \nab", &[Command::EnterInsert, Command::EnterNormal]);
        assert_eq!(
            text(&st),
            "    \nab",
            "a pre-existing whitespace line is preserved"
        );
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
        assert_eq!(text(&st), "ab\nX\n");
    }

    // Like `run`, but seeds the cursor at the end of `initial` (where a closer would be typed).
    fn run_at_end(initial: &str, cmds: &[Command]) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        st.set_cursor(BytePos(initial.len()));
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

    #[test]
    fn gi_column_zero_inserts_before_indentation() {
        // `gI` inserts at byte column 0, BEFORE the leading tab — unlike `I` (first non-blank). Oracle from
        // nvim v0.12.4: `gIX<Esc>` on `\thello` → `X\thello`.
        let st = run(
            "\thello",
            &[
                Command::Move(1, Motion::LineEnd),
                Command::InsertColumnZero,
                Command::InsertChar('X'),
                Command::EnterNormal,
            ],
        );
        assert_eq!(
            text(&st),
            "X\thello",
            "gI inserts at column 0, before the tab"
        );
        assert_eq!(
            st.cursor().0,
            0,
            "cursor rests on the inserted X after <Esc>"
        );
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
        assert_eq!(st.cursor().0, 4);
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
        assert_eq!(st.cursor().0, 4);
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
        assert_eq!(st.cursor().0, 5, "caret moved right by the inserted indent");
        // `<C-d>` on an indented line removes one shiftwidth; the caret rides left.
        let st = run("    x", &[Command::AppendLineEnd, Command::InsertDedent]);
        assert_eq!(text(&st), "x");
        assert_eq!(st.cursor().0, 1);
        // `<C-d>` with no indent is a no-op.
        let st = run("x", &[Command::EnterInsert, Command::InsertDedent]);
        assert_eq!(text(&st), "x");
    }

    #[test]
    fn insert_eval_splices_arithmetic_result_at_the_caret() {
        // `i` then `<C-r>=1+2*3<CR>` inserts "7" at the caret, staying in Insert (`:help i_CTRL-R`).
        let st = run(
            "xy",
            &[Command::EnterInsert, Command::InsertEval("1+2*3".into())],
        );
        assert_eq!(text(&st), "7xy");
        assert_eq!(st.mode(), Mode::Insert, "stays in Insert after <C-r>=");
        assert_eq!(st.cursor().0, 1, "cursor lands after the inserted result");
    }

    #[test]
    fn insert_eval_string_concat_and_empty_on_error() {
        // A string-concat expression inserts its result; a malformed one inserts nothing (Vim's degrade).
        let st = run(
            "",
            &[
                Command::EnterInsert,
                Command::InsertEval("'n='.5".into()),
                Command::InsertEval("1 +".into()), // parse error → empty → no-op
                Command::InsertEval("7 / 0".into()), // div-by-zero → empty → no-op
            ],
        );
        assert_eq!(
            text(&st),
            "n=5\n",
            "only the valid expression inserted text"
        );
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

    // `{count}gr{char}` — CLASSIC-Vim one-shot virtual replace. Byte oracles captured from nvim v0.12.4
    // (`vim -u NONE`); see the parity corpus for the driven-through-keys equivalents.

    #[test]
    fn gr_replaces_one_char_and_returns_to_normal() {
        // `gr` on `abcdef` with the cursor on `b` → `aXcdef`, cursor left on the replaced `X` (col 1).
        let st = run(
            "abcdef",
            &[
                Command::Move(1, Motion::Right),
                Command::VirtualReplaceChar(1, 'X'),
            ],
        );
        assert_eq!(text(&st), "aXcdef");
        assert_eq!(st.cursor().0, 1);
        assert_eq!(st.view.mode, Mode::Normal);
    }

    #[test]
    fn count_gr_replaces_count_chars() {
        // `3grZ` from col 0 → `ZZZdef`, cursor on the last `Z` (col 2).
        let st = run("abcdef", &[Command::VirtualReplaceChar(3, 'Z')]);
        assert_eq!(text(&st), "ZZZdef");
        assert_eq!(st.cursor().0, 2);
    }

    #[test]
    fn gr_over_a_tab_preserves_the_following_column() {
        // `grA` over a leading `<Tab>` (ts=4): the tab spans >1 virtual column, so `A` is inserted BEFORE it
        // (the tab shrinks) rather than replacing it — `A<Tab>X` — keeping `X` at its original column.
        let st = run("\tX", &[Command::VirtualReplaceChar(1, 'A')]);
        assert_eq!(text(&st), "A\tX");
        assert_eq!(st.cursor().0, 0);
    }

    #[test]
    fn count_gr_over_a_tab_inserts_then_walks_onto_it() {
        // `2grA` over `<Tab>X`: first `A` inserts before the (still multi-column) tab, the second `A` inserts
        // before the now-shrunk-but-still-multi-column tab → `AA<Tab>X`, cursor on the 2nd `A`.
        let st = run("\tX", &[Command::VirtualReplaceChar(2, 'A')]);
        assert_eq!(text(&st), "AA\tX");
        assert_eq!(st.cursor().0, 1);
    }

    #[test]
    fn gr_past_end_of_line_appends() {
        // `4grX` on `ab` overruns the 2-char line: it replaces `a`,`b` then APPENDS two more `X` past EOL
        // (virtual Replace grows the line) → `XXXX`. `r` would be a no-op here — this is the gr divergence.
        let st = run("ab", &[Command::VirtualReplaceChar(4, 'X')]);
        assert_eq!(text(&st), "XXXX");
        assert_eq!(st.cursor().0, 3);
    }
}
