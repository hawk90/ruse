//! #1302: the change marks `'[` `']` `'.` after the non-block operators, as nvim v0.12.6 leaves them (the
//! `cm_*` oracle fixtures and the `v_*` ones they came from).

#[cfg(test)]
mod change_mark_tests {
    use crate::editor::*;
    use crate::VisualOp;

    fn run(initial: &str, cursor: usize, cmds: &[Command]) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        st.view.indent.style = IndentStyle::Space;
        st.set_cursor(BytePos(cursor));
        for c in cmds {
            apply_command(&mut st, c);
        }
        st
    }

    /// `'[` `']` `'.` as `(line, byte col)`, 0-based; `'.` past the buffer's end is the line after its last.
    fn marks(st: &EditorState) -> [(usize, usize); 3] {
        let b = st.doc.bytes();
        let lc = |p: Option<BytePos>| {
            let p = p.expect("mark set").0;
            let ls = crate::motion::line_start(b, p);
            (crate::pos::raw::line_of(b, p), p - ls)
        };
        [
            lc(st.view.change_mark_start()),
            lc(st.view.change_mark_end()),
            lc(st.view.last_change()),
        ]
    }

    fn text(st: &EditorState) -> String {
        String::from_utf8(st.bytes().to_vec()).expect("utf8")
    }

    const SHIFT: &str = "abc def\nghi jkl\nmno\n";

    /// `op_shift`: `'[` at the raw column the operator started at, `']` on the last line's last byte, `'.`
    /// at column 0 of the first line, even for a shift that moves nothing.
    #[test]
    fn shift_marks_the_lines_from_where_it_started() {
        // `w>>`: '[ keeps the cursor's column, though the text moved right under it.
        let st = run(SHIFT, 4, &[Command::ShiftRight(1)]);
        assert_eq!(text(&st), "    abc def\nghi jkl\nmno\n");
        assert_eq!(marks(&st), [(0, 4), (0, 10), (0, 0)]);
        // `jw<k`: the motion's end comes first.
        let st = run(
            SHIFT,
            12,
            &[Command::ShiftMotion {
                left: true,
                count: 1,
                motion: Motion::Up,
            }],
        );
        assert_eq!(marks(&st), [(0, 4), (1, 6), (0, 0)]);
        // `w<<` at column 0 changes nothing, and still marks the line.
        let st = run(SHIFT, 4, &[Command::ShiftLeft(1)]);
        assert_eq!(text(&st), SHIFT);
        assert_eq!(marks(&st), [(0, 4), (0, 6), (0, 0)]);
        // `Vj>` over an empty last line: '] at its column 0.
        let st = run(
            "abc def\n\nmno\n",
            0,
            &[
                Command::EnterVisual {
                    kind: SelectKind::Linewise,
                },
                Command::MoveDown,
                Command::VisualOp(VisualOp::Shift {
                    left: false,
                    count: 1,
                }),
            ],
        );
        assert_eq!(marks(&st), [(0, 0), (1, 0), (0, 0)]);
    }

    /// `do_join`: `'[` where the first line ended, `']` past the joined line, `'.` at column 0 of the line
    /// after it, past the buffer's end when the join took the last line.
    #[test]
    fn join_marks_the_seam_and_the_line_after() {
        let st = run("a\nb\nc\n", 0, &[Command::JoinLines(3)]);
        assert_eq!(text(&st), "a b c\n");
        assert_eq!(marks(&st), [(0, 1), (0, 5), (1, 0)]);
        let st = run("a\nb\nc\n", 2, &[Command::JoinLines(2)]);
        assert_eq!(marks(&st), [(1, 1), (1, 3), (2, 0)]);
        let st = run("a\n  b\nc\n", 0, &[Command::JoinLinesNoSpace(2)]);
        assert_eq!(text(&st), "a  b\nc\n");
        assert_eq!(marks(&st), [(0, 1), (0, 4), (1, 0)]);
    }

    /// `op_format`: `']` on the cursor `gq` leaves, `'.` where its last change was: a join's line after,
    /// or the column a break split the line at.
    #[test]
    fn gq_marks_its_last_join_or_break() {
        let gqq = Command::Format {
            count: 1,
            motion: Motion::Line,
            keep_cursor: false,
        };
        let mut st = EditorState::new(b"aa bb cc dd ee\nff\n".to_vec());
        st.view.text_width = 5;
        apply_command(&mut st, &gqq);
        assert_eq!(text(&st), "aa bb\ncc dd\nee\nff\n");
        assert_eq!(marks(&st), [(0, 0), (2, 0), (1, 5)]);
        // `gqj` joins two lines: '. at column 0 of the line after the joined one.
        let st = run(
            "aa\nbb\ncc\n",
            0,
            &[Command::Format {
                count: 1,
                motion: Motion::Down,
                keep_cursor: false,
            }],
        );
        assert_eq!(text(&st), "aa bb\ncc\n");
        assert_eq!(marks(&st), [(0, 0), (0, 0), (1, 0)]);
    }

    /// `op_delete`: `'[` and `']` on where the operator started, `'.` at column 0 of the first deleted
    /// line, or of the line after the start for a charwise delete over lines.
    #[test]
    fn delete_marks_its_start() {
        // `3ldd`: the column is kept on the line that moves up.
        let st = run("abcdef\nxyzuvw\n", 3, &[Command::Delete(1, Motion::Line)]);
        assert_eq!(text(&st), "xyzuvw\n");
        assert_eq!(marks(&st), [(0, 3), (0, 3), (0, 0)]);
        // `Gdd`: the marks are on the deleted line's number, past the buffer's end.
        let st = run("abc\nd\nefg\n", 6, &[Command::Delete(1, Motion::Line)]);
        assert_eq!(text(&st), "abc\nd\n");
        assert_eq!(marks(&st), [(2, 0), (2, 0), (2, 0)]);
        // `lvjjd`: charwise over three lines, '. on the line after the start.
        let st = run(
            "abc\nd\nefg\n",
            1,
            &[
                Command::EnterVisual {
                    kind: SelectKind::Charwise,
                },
                Command::MoveDown,
                Command::MoveDown,
                Command::DeleteSelection,
            ],
        );
        assert_eq!(text(&st), "ag\n");
        assert_eq!(marks(&st), [(0, 1), (0, 1), (1, 0)]);
        // `x` within a line keeps the default marks.
        let st = run("abc\n", 1, &[Command::DeleteUnder(1)]);
        assert_eq!(marks(&st), [(0, 1), (0, 1), (0, 1)]);
    }

    /// `'[` `']` as nvim's `getpos()` columns `(line, raw col)`, 0-based: the offset's column plus how far
    /// the raw column lies past it (#1615).
    fn raw_change_marks(st: &EditorState) -> [(usize, usize); 2] {
        let b = st.doc.bytes();
        st.view.change_marks_raw().map(|m| {
            let (p, past) = m.expect("mark set");
            let ls = crate::motion::line_start(b, p.0);
            (crate::pos::raw::line_of(b, p.0), p.0 - ls + past)
        })
    }

    /// A linewise delete keeps `'[` `']` at the raw column it started at, past the end of the line now in
    /// the deleted one's place or on the line past the buffer's end (#1615); a jump to them clamps.
    #[test]
    fn linewise_delete_keeps_the_raw_column() {
        // `5ldd` over "abcdef": the marks are at column 5 of "gh", whose offsets end at column 2.
        let mut st = run("abcdef\ngh\nij\n", 5, &[Command::Delete(1, Motion::Line)]);
        assert_eq!(raw_change_marks(&st), [(0, 5), (0, 5)]);
        assert_eq!(
            st.view.change_mark_start(),
            Some(BytePos(2)),
            "the offset is on the line"
        );
        apply_command(&mut st, &Command::GotoChangeMarkStart);
        assert_eq!(st.cursor().0, 1, "`[ rests on the line's last char");
        // `j3ldd` on the last line: the marks are on the line past the buffer's end, column 3.
        let st = run("ab\ncdef\n", 6, &[Command::Delete(1, Motion::Line)]);
        assert_eq!(text(&st), "ab\n");
        assert_eq!(raw_change_marks(&st), [(1, 3), (1, 3)]);
        // `3ldd` onto "éé": column 3 is inside the second char; the offset is on its first byte.
        let st = run(
            "abcdef\n\u{e9}\u{e9}\n",
            3,
            &[Command::Delete(1, Motion::Line)],
        );
        assert_eq!(raw_change_marks(&st), [(0, 3), (0, 3)]);
        assert_eq!(st.view.change_mark_start(), Some(BytePos(2)));
        // `j5ldk`: the operator starts where `k` left the cursor, on the last char of "ab", not past it.
        let st = run("ab\nabcdef\nij\n", 8, &[Command::Delete(1, Motion::Up)]);
        assert_eq!(text(&st), "ij\n");
        assert_eq!(raw_change_marks(&st), [(0, 1), (0, 1)]);
        // A later change sets fresh marks with no raw column of their own.
        let mut st = run("abcdef\ngh\n", 5, &[Command::Delete(1, Motion::Line)]);
        apply_command(&mut st, &Command::DeleteUnder(1));
        assert_eq!(raw_change_marks(&st), [(0, 1), (0, 1)]);
    }

    /// An inclusive yank and a shift put `']` on a multibyte char's LAST byte, as nvim's `getpos()` reports
    /// it (#1613); an exclusive yank and a put keep it on the char's first byte. The offset stays on the
    /// char, so `` `] `` lands on it.
    #[test]
    fn inclusive_end_is_the_last_byte() {
        // `y$` over "ab中" (中 is bytes 2..5): '] at column 4.
        let mut st = run("ab\u{4e2d}\n", 0, &[Command::Yank(1, Motion::LineEnd)]);
        assert_eq!(raw_change_marks(&st), [(0, 0), (0, 4)]);
        assert_eq!(st.view.change_mark_end(), Some(BytePos(2)));
        apply_command(&mut st, &Command::GotoChangeMarkEnd);
        assert_eq!(st.cursor().0, 2, "`] rests on the char's first byte");
        // `y2l` over "a中中b": exclusive, '] on the first byte of the last char taken.
        let st = run(
            "a\u{4e2d}\u{4e2d}b\n",
            0,
            &[Command::Yank(2, Motion::Right)],
        );
        assert_eq!(raw_change_marks(&st), [(0, 0), (0, 1)]);
        // `vly` over "a😀b" (😀 is bytes 1..5): '] at column 4.
        let st = run(
            "a\u{1f600}b\n",
            0,
            &[
                Command::EnterVisual {
                    kind: SelectKind::Charwise,
                },
                Command::Move(1, Motion::Right),
                Command::YankSelection,
            ],
        );
        assert_eq!(raw_change_marks(&st), [(0, 0), (0, 4)]);
        // `>>` on "é" with 'sw' 4: "    é", '] on its last byte.
        let st = run("\u{e9}\n", 0, &[Command::ShiftRight(1)]);
        assert_eq!(raw_change_marks(&st), [(0, 0), (0, 5)]);
        // `yyjp`: a linewise put's '] is on the first byte of the last line's last char.
        let st = run(
            "ab\u{4e2d}\nx\n",
            0,
            &[
                Command::Yank(1, Motion::Line),
                Command::Paste {
                    after: true,
                    count: 1,
                    move_after: false,
                },
            ],
        );
        assert_eq!(raw_change_marks(&st), [(1, 0), (1, 2)]);
    }

    /// A linewise put's `']` is on the last put line's last char; a charwise yank that takes a newline ends
    /// `']` on the char before it.
    #[test]
    fn put_and_yank_end_on_a_char() {
        let st = run(
            "abc\nde\n",
            0,
            &[
                Command::Yank(2, Motion::Line),
                Command::Paste {
                    after: false,
                    count: 1,
                    move_after: false,
                },
            ],
        );
        assert_eq!(text(&st), "abc\nde\nabc\nde\n");
        assert_eq!(marks(&st), [(0, 0), (1, 1), (0, 0)]);
        let st = run(
            "abcdef\nxyz\n",
            1,
            &[
                Command::EnterVisual {
                    kind: SelectKind::Charwise,
                },
                Command::Move(1, Motion::LineEnd),
                Command::YankSelection,
            ],
        );
        let b = st.doc.bytes();
        let end = st.view.change_mark_end().expect("'] set").0;
        assert_eq!(st.view.change_mark_start(), Some(BytePos(1)));
        assert_eq!((crate::pos::raw::line_of(b, end), end), (0, 5));
    }

    /// A char typed in Insert puts `'.` on that char, where nvim's `ins_char` calls `changed_bytes`, not on
    /// the caret after it.
    #[test]
    fn typed_char_dot_is_on_the_char() {
        let st = run(
            "abcd\n",
            1,
            &[
                Command::EnterInsert,
                Command::InsertChar('x'),
                Command::InsertChar('y'),
                Command::EnterNormal,
            ],
        );
        assert_eq!(text(&st), "axybcd\n");
        assert_eq!(marks(&st), [(0, 1), (0, 3), (0, 2)]);
    }

    /// A join that took the last line leaves `'.` on the line past the buffer's end: `` `. ``, `'.` and `g;`
    /// there fail as nvim's `E19` and the cursor stays; any later change clears it.
    #[test]
    fn a_dot_past_the_end_is_no_line() {
        for jump in [
            Command::GotoLastChange,
            Command::GotoLastChangeLine,
            Command::GotoOlderChange,
        ] {
            let mut st = run("a\nb\n", 0, &[Command::JoinLines(2)]);
            assert_eq!(marks(&st)[2], (1, 0));
            let effects = apply_command(&mut st, &jump);
            assert!(
                effects
                    .iter()
                    .any(|e| matches!(e, Effect::Status(m) if m.starts_with("E19"))),
                "{jump:?}: {effects:?}"
            );
            assert_eq!(st.cursor().0, 1, "{jump:?} leaves the cursor on the seam");
        }
        // `x` on an unterminated last line's only char leaves the same bytes, but that line is real: no E19.
        let mut st = run("a\nb", 2, &[Command::DeleteUnder(1)]);
        apply_command(&mut st, &Command::Move(1, Motion::Up));
        let effects = apply_command(&mut st, &Command::GotoLastChange);
        assert!(!effects.iter().any(|e| matches!(e, Effect::Status(_))));
    }

    const VISUAL_P: Command = Command::PasteSelection {
        swap: true,
        count: 1,
        after: true,
        move_after: false,
    };

    /// The last Visual area as `(start, end)` byte offsets and its kind.
    fn visual_area(st: &EditorState) -> (usize, usize, SelectKind) {
        let (s, e, kind) = st.view.last_visual_bounds().expect("a last Visual area");
        (s.0, e.0, kind)
    }

    /// `yiw`, `w`, `viw`, `p`: a charwise Visual put of the first word over the second.
    fn yiw_w_viw_p(initial: &str) -> EditorState {
        run(
            initial,
            0,
            &[
                Command::Yank(1, Motion::InnerWord),
                Command::Move(1, Motion::WordFwd),
                Command::EnterVisual {
                    kind: SelectKind::Charwise,
                },
                Command::Move(1, Motion::InnerWord),
                VISUAL_P,
            ],
        )
    }

    /// #1614: a Visual put leaves `'<` `'>` on its `'[` `']` in the selection's kind, and `'.` where nvim's
    /// `do_put` last changed text; undo brings the replaced area back and redo the put's (nvim v0.12.6,
    /// the `v_put_marks_*` fixtures).
    #[test]
    fn visual_put_sets_the_visual_marks_to_the_put() {
        // `yiwwviwp` on `ab cdef gh`: `cdef` becomes `ab`, '< '> = '[ '] = (0,3)-(0,4), '. on '[.
        let mut st = yiw_w_viw_p("ab cdef gh\n");
        assert_eq!(text(&st), "ab ab gh\n");
        assert_eq!(marks(&st), [(0, 3), (0, 4), (0, 3)]);
        assert_eq!(visual_area(&st), (3, 4, SelectKind::Charwise));
        apply_command(&mut st, &Command::Undo);
        assert_eq!(visual_area(&st), (3, 6, SelectKind::Charwise));
        apply_command(&mut st, &Command::Redo);
        assert_eq!(visual_area(&st), (3, 4, SelectKind::Charwise));
        // A replaced text sharing its ends with the put one still marks the whole put: `ab` over `ac`.
        let st = yiw_w_viw_p("ab ac\n");
        assert_eq!(text(&st), "ab ab\n");
        assert_eq!(marks(&st), [(0, 3), (0, 4), (0, 3)]);
        assert_eq!(visual_area(&st), (3, 4, SelectKind::Charwise));
    }

    /// #1614: a one-line charwise register over a block marks the top row's put text, and `'.` the last
    /// row's; the area stays blockwise.
    #[test]
    fn visual_block_put_marks_the_top_row() {
        // `yiwl<C-v>jp` on `abc/def/ghi`: nvim gives `aabcc/dabcf/ghi`, '[ '] (0,1)-(0,3), '. (1,1).
        let st = run(
            "abc\ndef\nghi\n",
            0,
            &[
                Command::Yank(1, Motion::InnerWord),
                Command::MoveRight,
                Command::EnterVisual {
                    kind: SelectKind::Blockwise,
                },
                Command::MoveDown,
                VISUAL_P,
            ],
        );
        assert_eq!(text(&st), "aabcc\ndabcf\nghi\n");
        assert_eq!(marks(&st), [(0, 1), (0, 3), (1, 1)]);
        assert_eq!(visual_area(&st), (1, 3, SelectKind::Blockwise));
    }
}
