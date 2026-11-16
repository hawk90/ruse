//! The Emacs profile: kill, mark and region, caret gravity, and the emptied line. Split from `editor/tests.rs` (#1397).

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
        joined.set_cursor(BytePos(2)); // on the '\n'
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
        back.set_cursor(BytePos(10)); // somewhere in "beta"
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
        mid.set_cursor(BytePos(0));
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
        eob.set_cursor(BytePos(3));
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
        mid.set_cursor(BytePos(1));
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
        eol.set_cursor(BytePos(3));
        apply_command(&mut eol, &Command::EmacsTransposeChars);
        assert_eq!(text(&eol), "acb");
        assert_eq!(eol.view.cursor, 3);

        // No pair (point at buffer start): inert.
        let mut bob = EditorState::new(b"abc".to_vec());
        bob.set_caret_gravity(CaretGravity::BetweenChar);
        bob.set_cursor(BytePos(0));
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
        cap.set_cursor(BytePos(0));
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
        up.set_cursor(BytePos(0));
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
        down.set_cursor(BytePos(0));
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
        none.set_cursor(BytePos(3));
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
        kl.set_cursor(BytePos(0));
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
        kw.set_cursor(BytePos(0));
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
        brk.set_cursor(BytePos(0));
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

    // Emacs just-one-space (M-SPC) / delete-horizontal-space (M-\) (D-051): collapse surrounding spaces/tabs.
    #[test]
    fn emacs_horizontal_space_collapses_surrounding_whitespace() {
        let mut one = EditorState::new(b"foo   bar".to_vec());
        one.set_caret_gravity(CaretGravity::BetweenChar);
        one.set_cursor(BytePos(4)); // inside the three spaces
        apply_command(&mut one, &Command::EmacsHorizontalSpace { keep_one: true });
        assert_eq!(text(&one), "foo bar");
        assert_eq!(one.view.cursor, 4, "point rests after the single space");

        let mut none = EditorState::new(b"foo   bar".to_vec());
        none.set_caret_gravity(CaretGravity::BetweenChar);
        none.set_cursor(BytePos(4));
        apply_command(
            &mut none,
            &Command::EmacsHorizontalSpace { keep_one: false },
        );
        assert_eq!(text(&none), "foobar");
        assert_eq!(none.view.cursor, 3);

        // just-one-space with no surrounding whitespace inserts one.
        let mut ins = EditorState::new(b"ab".to_vec());
        ins.set_caret_gravity(CaretGravity::BetweenChar);
        ins.set_cursor(BytePos(1));
        apply_command(&mut ins, &Command::EmacsHorizontalSpace { keep_one: true });
        assert_eq!(text(&ins), "a b");
        assert_eq!(ins.view.cursor, 2);
    }

    // Emacs open-line (C-o) (D-051): insert a newline at point, leaving point before it. No mode change.
    #[test]
    fn emacs_open_line_inserts_newline_and_keeps_point() {
        let mut st = EditorState::new(b"foobar".to_vec());
        st.set_caret_gravity(CaretGravity::BetweenChar);
        st.set_cursor(BytePos(3));
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
        one.set_cursor(BytePos(7));
        apply_command(&mut one, &Command::EmacsBackwardKillWord { count: 1 });
        assert_eq!(text(&one), "foo ");
        assert_eq!(one.view.cursor, 4);
        assert_eq!(one.register().text(), b"bar");

        // Two backward kills: the second PREPENDS ("bar " before "baz" -> "bar baz").
        let mut acc = EditorState::new(b"foo bar baz".to_vec());
        acc.set_caret_gravity(CaretGravity::BetweenChar);
        acc.set_cursor(BytePos(11));
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
        a.set_cursor(BytePos(3));
        apply_command(&mut a, &Command::EmacsTransposeWords);
        assert_eq!(text(&a), "bar foo");
        assert_eq!(a.view.cursor, 7);
        assert!(a.register().is_empty());

        // Point inside the middle word of three swaps that word with the NEXT one (matches Emacs).
        let mut b = EditorState::new(b"foo bar baz".to_vec());
        b.set_caret_gravity(CaretGravity::BetweenChar);
        b.set_cursor(BytePos(5)); // inside "bar"
        apply_command(&mut b, &Command::EmacsTransposeWords);
        assert_eq!(text(&b), "foo baz bar");
        assert_eq!(b.view.cursor, 11);
    }

    // Emacs `forward-word` / `backward-word` (M-f / M-b) use a TWO-class syntax split (word constituent vs
    // non-word), where a punctuation run is NOT its own word (as it is under Vim `e`/`b`) and `_` is a
    // NON-word char in fundamental-mode. All values below are pinned to GNU Emacs 30.2 via the parity oracle
    // (tests/parity/emacs/fixtures/corpus.yaml); this hard-asserts the emacs-matching bytes/point so a
    // regression back to the Vim word helpers fails the build, not just the (non-gating) parity tally.
    #[test]
    fn emacs_word_motions_are_two_class_over_punct_and_underscore() {
        // forward-word skips a leading punctuation run then moves over the word: "...foo" -> point 6.
        let mut fw = EditorState::new(b"...foo".to_vec());
        fw.set_caret_gravity(CaretGravity::BetweenChar);
        fw.set_cursor(BytePos(0));
        apply_command(&mut fw, &Command::Move(1, Motion::EmacsWordFwd));
        assert_eq!(fw.view.cursor, 6);

        // `_` is non-word: forward-word on "foo_bar" stops after "foo" (point 3).
        let mut us = EditorState::new(b"foo_bar".to_vec());
        us.set_caret_gravity(CaretGravity::BetweenChar);
        us.set_cursor(BytePos(0));
        apply_command(&mut us, &Command::Move(1, Motion::EmacsWordFwd));
        assert_eq!(us.view.cursor, 3);

        // backward-word skips a trailing punctuation run then moves over the word: from inside "bar" of
        // "foo.bar" it lands at the start of "foo" (point 0), NOT at the "." where Vim `b` would stop.
        let mut bw = EditorState::new(b"foo.bar".to_vec());
        bw.set_caret_gravity(CaretGravity::BetweenChar);
        bw.set_cursor(BytePos(4));
        apply_command(&mut bw, &Command::Move(1, Motion::EmacsWordBack));
        assert_eq!(bw.view.cursor, 0);

        // kill-word inherits the two-class span: on "foo_bar" it kills just "foo" (stops at `_`).
        let mut kw = EditorState::new(b"foo_bar".to_vec());
        kw.set_caret_gravity(CaretGravity::BetweenChar);
        kw.set_cursor(BytePos(0));
        apply_command(&mut kw, &Command::EmacsKillWord { count: 1 });
        assert_eq!(text(&kw), "_bar");
        assert_eq!(kw.register().text(), b"foo");

        // upcase-word too: recases only "foo" of "foo_bar", leaving "_bar" untouched, point after "foo".
        let mut up = EditorState::new(b"foo_bar".to_vec());
        up.set_caret_gravity(CaretGravity::BetweenChar);
        up.set_cursor(BytePos(0));
        apply_command(
            &mut up,
            &Command::EmacsCaseWord {
                case: WordCase::Upcase,
            },
        );
        assert_eq!(text(&up), "FOO_bar");
        assert_eq!(up.view.cursor, 3);

        // backward-kill-word crossing a punctuation run: from inside "bar" of "foo.bar" it kills "foo."
        // (back to the start of "foo"), leaving "bar".
        let mut bkw = EditorState::new(b"foo.bar".to_vec());
        bkw.set_caret_gravity(CaretGravity::BetweenChar);
        bkw.set_cursor(BytePos(4));
        apply_command(&mut bkw, &Command::EmacsBackwardKillWord { count: 1 });
        assert_eq!(text(&bkw), "bar");
        assert_eq!(bkw.view.cursor, 0);
        assert_eq!(bkw.register().text(), b"foo.");
    }

    // Emacs mark-word (M-@) (D-051): set the mark at the end of the next word, point unchanged.
    #[test]
    fn emacs_mark_word_sets_mark_at_word_end() {
        let mut st = EditorState::new(b"foo bar".to_vec());
        st.set_caret_gravity(CaretGravity::BetweenChar);
        st.set_cursor(BytePos(0));
        apply_command(&mut st, &Command::EmacsMarkWord);
        assert_eq!(st.view.cursor, 0, "point does not move");
        assert_eq!(
            st.mark(),
            Some(BytePos(3)),
            "mark lands at the end of 'foo'"
        );
        assert!(st.register().is_empty());
    }

    // Emacs kill-whole-line (C-S-DEL) (D-051): kill the whole line incl. its newline regardless of column,
    // into the register (accumulating), point at the line start.
    #[test]
    fn emacs_kill_whole_line_kills_the_line_and_newline() {
        let mut st = EditorState::new(b"foo\nbar".to_vec());
        st.set_caret_gravity(CaretGravity::BetweenChar);
        st.set_cursor(BytePos(1)); // mid-line — column is irrelevant
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
        up.set_cursor(BytePos(0));
        apply_command(&mut up, &Command::SetMark); // mark at 0
        up.set_cursor(BytePos(3)); // region [0,3) = "foo"
        apply_command(
            &mut up,
            &Command::EmacsCaseRegion {
                case: WordCase::Upcase,
            },
        );
        assert_eq!(text(&up), "FOO bar");
        assert_eq!(
            (up.view.cursor, up.mark()),
            (3, Some(BytePos(0))),
            "point and mark unchanged"
        );
        assert!(up.register().is_empty());

        // No mark set: inert.
        let mut none = EditorState::new(b"foo".to_vec());
        none.set_caret_gravity(CaretGravity::BetweenChar);
        none.set_cursor(BytePos(3));
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
            st.set_cursor(BytePos(cur));
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

    /// #617: `'autoindent'` and the `'smarttab'` `<BS>` are Vim-profile rules; an Emacs `newline` and
    /// `delete-backward-char` keep their plain behaviour under the same (default) options.
    #[test]
    fn emacs_newline_and_backspace_ignore_vim_indent_rules() {
        let mut st = EditorState::new(b"    foo".to_vec());
        st.set_caret_gravity(CaretGravity::BetweenChar);
        apply_command(&mut st, &Command::MoveLineEnd);
        apply_command(&mut st, &Command::InsertNewline);
        assert_eq!(st.doc.bytes(), b"    foo\n");
        let mut st = EditorState::new(b"    foo".to_vec());
        st.set_caret_gravity(CaretGravity::BetweenChar);
        apply_command(&mut st, &Command::EnterInsert);
        st.view.cursor = 4;
        apply_command(&mut st, &Command::DeleteBack);
        assert_eq!(st.doc.bytes(), b"   foo");
    }

    /// #618: auto-wrap and the comment-leader opens are Vim-profile rules; Emacs typing past
    /// 'textwidth' and an Emacs `newline` on a comment line stay plain.
    #[test]
    fn emacs_typing_does_not_auto_wrap_or_continue_comments() {
        let mut st = EditorState::new(b"aaa bbb".to_vec());
        st.set_caret_gravity(CaretGravity::BetweenChar);
        st.set_option(EditorOption::TextWidth(5));
        st.set_option(EditorOption::FormatOptions("tcqjro".into()));
        apply_command(&mut st, &Command::MoveLineEnd);
        apply_command(&mut st, &Command::InsertChar('x'));
        assert_eq!(st.doc.bytes(), b"aaa bbbx");
        let mut st = EditorState::new(b"// a".to_vec());
        st.set_caret_gravity(CaretGravity::BetweenChar);
        st.set_option(EditorOption::FormatOptions("tcqjro".into()));
        apply_command(&mut st, &Command::MoveLineEnd);
        apply_command(&mut st, &Command::InsertNewline);
        assert_eq!(st.doc.bytes(), b"// a\n");
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
        st.set_cursor(BytePos(start));
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
            vim.cursor().0,
            5,
            "Vim clamps the Normal-mode caret onto the last char"
        );
        let emacs = run_with(CaretGravity::BetweenChar, "hello world", 6, &cmds);
        assert_eq!(
            emacs.cursor().0,
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
        assert_eq!(vim.cursor().0, 3, "Vim paste rests on the last pasted byte");
        let emacs = run_with(CaretGravity::BetweenChar, "abc", 0, &cmds);
        assert_eq!(String::from_utf8(emacs.bytes().to_vec()).unwrap(), "aabcbc");
        assert_eq!(
            emacs.cursor().0,
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
            st.cursor().0,
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

/// RFC-0024 §5 is Vim's line model: the Emacs profile's empty buffer has no line, so `:r` and `:put` into it
/// give the lines as before, and never make (or recurse on) the emptied line.
#[test]
fn the_emacs_profile_has_no_emptied_line() {
    use crate::editor::substitute::LineAddr;
    let mut st = crate::editor::EditorState::new(Vec::new());
    st.set_caret_gravity(crate::CaretGravity::BetweenChar);
    assert_eq!(st.read_lines(LineAddr::Line(0), b"\xff\n", false), 1);
    assert_eq!(st.bytes(), b"\xff");
}
