//! `:r`/filter, `:m`/`:t` relocation and the history list. Split from `editor/tests.rs` (#1397).

/// `:r`/`:read` line-read + `:{range}!{cmd}` filter core splicing. The frontend does the IO (fs / shell) and
/// hands the bytes to these pure methods. Ground truth captured from nvim v0.12.4 (`nvim -u NONE` headless,
/// `writefile(getline(1,'$'))` + `line('.')` / `col('.')`): `:r file` leaves the cursor on the FIRST inserted
/// line, `:r !cmd` on the LAST, and a `:{range}!` filter on the first line of the replaced region.
#[cfg(test)]
mod read_filter_tests {
    use crate::editor::*;

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

    /// `:2r file` inserts the file's lines below line 2; the cursor lands on the FIRST inserted line. nvim:
    /// main=alpha/beta/gamma, ins=X1/X2, `:2r` → alpha/beta/X1/X2/gamma, cursor (2,0) [1-based (3,1)].
    #[test]
    fn read_file_below_addressed_line_cursor_on_first() {
        let mut st = EditorState::new(b"alpha\nbeta\ngamma\n".to_vec());
        assert_eq!(st.read_lines(LineAddr::Line(2), b"X1\nX2\n", false), 2);
        assert_eq!(text(&st), "alpha\nbeta\nX1\nX2\ngamma\n");
        assert_eq!(
            cursor_line_col(&st),
            (2, 0),
            "cursor on the first inserted line"
        );
    }

    /// `:0r file` inserts at the very top, before line 1; the cursor lands on the new top line. nvim: `:0r`
    /// with cursor on line 3 → X1/X2/alpha/beta/gamma, cursor (0,0) [1-based (1,1)].
    #[test]
    fn read_at_line_zero_inserts_at_the_top() {
        let mut st = EditorState::new(b"alpha\nbeta\ngamma\n".to_vec());
        st.set_cursor(BytePos(12)); // gamma, to prove `:0r` ignores the cursor line
        assert_eq!(st.read_lines(LineAddr::Line(0), b"X1\nX2\n", false), 2);
        assert_eq!(text(&st), "X1\nX2\nalpha\nbeta\ngamma\n");
        assert_eq!(cursor_line_col(&st), (0, 0));
    }

    /// A bare `:r` (LineAddr::Current) reads below the cursor's line. nvim: cursor on line 1, `:r` → the
    /// lines land between line 1 and line 2.
    #[test]
    fn bare_read_inserts_after_the_current_line() {
        let mut st = EditorState::new(b"alpha\nbeta\n".to_vec());
        st.set_cursor(BytePos(0)); // alpha
        assert_eq!(st.read_lines(LineAddr::Current, b"X1\nX2\n", false), 2);
        assert_eq!(text(&st), "alpha\nX1\nX2\nbeta\n");
        assert_eq!(cursor_line_col(&st), (1, 0));
    }

    /// `:$r file` appends the file after the last line. nvim: `:$r ins` → alpha/beta/X1/X2, cursor (2,0).
    #[test]
    fn read_at_last_line_appends() {
        let mut st = EditorState::new(b"alpha\nbeta\n".to_vec());
        assert_eq!(st.read_lines(LineAddr::Last, b"X1\nX2\n", false), 2);
        assert_eq!(text(&st), "alpha\nbeta\nX1\nX2\n");
        assert_eq!(cursor_line_col(&st), (2, 0));
    }

    /// The cursor lands on the FIRST NON-BLANK of the first inserted line. nvim: `:r` of "  indented\nX2"
    /// at line 1 → cursor (1,2) [1-based (2,3)].
    #[test]
    fn read_cursor_lands_on_first_non_blank() {
        let mut st = EditorState::new(b"alpha\nbeta\n".to_vec());
        st.set_cursor(BytePos(0)); // alpha
        assert_eq!(
            st.read_lines(LineAddr::Current, b"  indented\nX2\n", false),
            2
        );
        assert_eq!(text(&st), "alpha\n  indented\nX2\nbeta\n");
        assert_eq!(cursor_line_col(&st), (1, 2), "past the two leading spaces");
    }

    /// A file with NO trailing newline still reads its last line (no spurious blank). nvim: read "no_nl_a\n
    /// no_nl_b" (noeol) below line 1 → alpha/no_nl_a/no_nl_b/beta.
    #[test]
    fn read_file_without_trailing_newline_keeps_its_last_line() {
        let mut st = EditorState::new(b"alpha\nbeta\n".to_vec());
        st.set_cursor(BytePos(0));
        assert_eq!(
            st.read_lines(LineAddr::Current, b"no_nl_a\nno_nl_b", false),
            2
        );
        assert_eq!(text(&st), "alpha\nno_nl_a\nno_nl_b\nbeta\n");
    }

    /// The COMMAND form (`:r !cmd`, `cursor_on_last = true`) leaves the cursor on the LAST inserted line — a
    /// real nvim quirk that `:r file` does not share. nvim: `:r !printf 'C1\nC2\n'` at line 1 → cursor (2,0)
    /// [1-based (3,1)], on C2.
    #[test]
    fn read_command_leaves_cursor_on_last_line() {
        let mut st = EditorState::new(b"alpha\nbeta\n".to_vec());
        st.set_cursor(BytePos(0)); // alpha
        assert_eq!(st.read_lines(LineAddr::Current, b"C1\nC2\n", true), 2);
        assert_eq!(text(&st), "alpha\nC1\nC2\nbeta\n");
        assert_eq!(
            cursor_line_col(&st),
            (2, 0),
            "cursor on the LAST inserted line"
        );
    }

    /// An empty read is a no-op leaving the buffer + cursor untouched.
    #[test]
    fn empty_read_is_a_no_op() {
        let mut st = EditorState::new(b"alpha\nbeta\n".to_vec());
        st.set_cursor(BytePos(6)); // beta
        assert_eq!(st.read_lines(LineAddr::Current, b"", false), 0);
        assert_eq!(text(&st), "alpha\nbeta\n");
        assert_eq!(cursor_line_col(&st), (1, 0));
    }

    /// A read is ONE undo group: a single undo removes every inserted line.
    #[test]
    fn read_is_one_undo_group() {
        let mut st = EditorState::new(b"alpha\nbeta\n".to_vec());
        st.read_lines(LineAddr::Line(1), b"X1\nX2\n", false);
        assert_eq!(text(&st), "alpha\nX1\nX2\nbeta\n");
        apply_command(&mut st, &Command::Undo);
        assert_eq!(
            text(&st),
            "alpha\nbeta\n",
            "one undo removes the whole read"
        );
    }

    /// `range_text` returns the range's lines with trailing newlines, as fed to a filter's stdin.
    #[test]
    fn range_text_yields_the_range_lines_with_newlines() {
        let mut st = EditorState::new(b"one\ntwo\nthree\nfour\n".to_vec());
        assert_eq!(
            st.range_text(SubRange::Lines(2, 3)).as_deref(),
            Some(&b"two\nthree\n"[..])
        );
        assert_eq!(
            st.range_text(SubRange::WholeFile).as_deref(),
            Some(&b"one\ntwo\nthree\nfour\n"[..]),
            "the whole-file range does not include a phantom trailing empty line"
        );
        st.set_cursor(BytePos(0));
        assert_eq!(
            st.range_text(SubRange::CurrentLine).as_deref(),
            Some(&b"one\n"[..])
        );
    }

    /// #875: a buffer that is not UTF-8 still feeds its bytes to a filter, and takes the output back.
    #[test]
    fn range_text_and_filter_keep_non_utf8_bytes() {
        let mut st = EditorState::new(b"a\xff\nb\n".to_vec());
        assert_eq!(
            st.range_text(SubRange::WholeFile).as_deref(),
            Some(&b"a\xff\nb\n"[..])
        );
        assert_eq!(st.filter_lines(SubRange::Lines(1, 1), b"\xfe\n"), 1);
        assert_eq!(st.doc.bytes(), b"\xfe\nb\n");
    }

    /// `:{range}!cmd` replaces the range's lines with the filter output; the cursor lands on the first line of
    /// the region. nvim: `:2,3!tr a-z A-Z` on one/two/three/four → one/TWO/THREE/four, cursor (1,0).
    #[test]
    fn filter_replaces_range_cursor_on_first_line() {
        let mut st = EditorState::new(b"one\ntwo\nthree\nfour\n".to_vec());
        assert_eq!(st.filter_lines(SubRange::Lines(2, 3), b"TWO\nTHREE\n"), 2);
        assert_eq!(text(&st), "one\nTWO\nTHREE\nfour\n");
        assert_eq!(cursor_line_col(&st), (1, 0));
    }

    /// A filter that emits FEWER lines than it consumed shortens the buffer (nvim: `:%!grep keep` drops the
    /// non-matching line). The `input_count` return is the number of INPUT lines filtered.
    #[test]
    fn filter_can_shorten_the_buffer() {
        let mut st = EditorState::new(b"keep1\ndrop\nkeep2\n".to_vec());
        assert_eq!(
            st.filter_lines(SubRange::WholeFile, b"keep1\nkeep2\n"),
            3,
            "three input lines were filtered"
        );
        assert_eq!(text(&st), "keep1\nkeep2\n");
        assert_eq!(cursor_line_col(&st), (0, 0));
    }

    /// A filter that emits NOTHING deletes the range's lines (Vim: a filter producing no output).
    #[test]
    fn filter_to_empty_deletes_the_range() {
        let mut st = EditorState::new(b"one\ntwo\nthree\n".to_vec());
        assert_eq!(st.filter_lines(SubRange::Lines(2, 2), b""), 1);
        assert_eq!(text(&st), "one\nthree\n");
    }
}

#[cfg(test)]
mod relocate_tests {
    //! #1257: `:m`/`:t` edit only the block they move or copy. Every shape of a small buffer gives the
    //! text the line model gives, and one `u` restores it.
    use crate::editor::*;

    /// Every buffer over `a`, `b` and `\n` up to `max` bytes.
    fn buffers(max: usize) -> Vec<Vec<u8>> {
        let mut out = vec![Vec::new()];
        let mut layer = vec![Vec::new()];
        for _ in 0..max {
            layer = layer
                .iter()
                .flat_map(|b: &Vec<u8>| {
                    (*b"ab\n").map(|c| {
                        let mut v = b.clone();
                        v.push(c);
                        v
                    })
                })
                .collect();
            out.extend(layer.iter().cloned());
        }
        out
    }

    #[test]
    fn move_and_copy_match_the_line_model_on_every_small_buffer() {
        for old in buffers(5).into_iter().filter(|b| !b.is_empty()) {
            let n = line_count(&EditorState::new(old.clone()).doc);
            for first in 0..n {
                for last in first..n {
                    for ins in 0..=n {
                        for copy in [false, true] {
                            let mut st = EditorState::new(old.clone());
                            let range = SubRange::Lines(first + 1, last + 1);
                            let dest = LineAddr::Line(ins);
                            let r = if copy {
                                st.copy_lines(range, dest)
                            } else {
                                st.move_lines(range, dest)
                            };
                            let what = format!("{old:?} {first}..={last} -> {ins} copy={copy}");
                            if r.is_none() || r == Some(0) {
                                assert_eq!(st.doc.bytes(), old.as_slice(), "{what}");
                                continue;
                            }
                            let want = crate::editor::line_cmds::relocated_by_lines(
                                &old, first, last, ins, copy,
                            );
                            assert_eq!(st.doc.bytes(), want.as_slice(), "{what}");
                            assert!(st.view.cursor <= want.len(), "{what}");
                            st.doc.undo();
                            assert_eq!(st.doc.bytes(), old.as_slice(), "{what}: u");
                        }
                    }
                }
            }
        }
    }

    /// The copy's edit is the one [`crate::Edit::lines_changed`] finds over the whole rebuilt buffer.
    #[test]
    fn lines_changed_by_insert_is_lines_changed() {
        let texts = buffers(3);
        for old in buffers(6) {
            for at in 0..=old.len() {
                for text in &texts {
                    let mut new = old.clone();
                    new.splice(at..at, text.iter().copied());
                    let want = crate::Edit::lines_changed(&old, new);
                    let got = crate::Edit::lines_changed_by_insert(&old, at, text.clone());
                    assert_eq!(got, want, "{old:?} + {text:?} at {at}");
                }
            }
        }
    }
}

#[cfg(test)]
mod history_list_tests {
    use crate::editor::*;
    use crate::motion::Motion;

    fn run(initial: &str, cmds: &[Command]) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        for c in cmds {
            apply_command(&mut st, c);
        }
        st
    }

    /// #722: typing a word is one undo step, so it is one change-list entry, not one per keystroke; a
    /// change on another line after an undo sync is a new entry, and a third `g;` is past the oldest.
    #[test]
    fn change_list_takes_an_entry_per_undo_step() {
        let mut cmds = vec![Command::EnterInsert];
        cmds.extend("hello".chars().map(Command::InsertChar));
        cmds.push(Command::EnterNormal);
        let mut st = run("abc\ndef", &cmds);
        assert_eq!(st.changes_snapshot().len(), 1, "one insert, one entry");
        st.doc.sync_undo();
        apply_command(&mut st, &Command::Move(1, Motion::Down));
        apply_command(&mut st, &Command::DeleteUnder(1));
        assert_eq!(st.changes_snapshot().len(), 2, "a change on another line");
        apply_command(&mut st, &Command::GotoOlderChange);
        apply_command(&mut st, &Command::GotoOlderChange);
        let effects = apply_command(&mut st, &Command::GotoOlderChange);
        assert!(
            effects.contains(&Effect::Status("E662: At start of changelist".into())),
            "{effects:?}"
        );
    }

    /// #940: `g;` on an empty change list is E664; `CTRL-O` on an empty jumplist fails with no message.
    #[test]
    fn empty_lists_fail() {
        let mut st = EditorState::new(b"abc".to_vec());
        let effects = apply_command(&mut st, &Command::GotoOlderChange);
        assert!(effects.contains(&Effect::Status("E664: Changelist is empty".into())));
        assert!(effects.contains(&Effect::Beep));
        let effects = apply_command(&mut st, &Command::GotoOlderJump);
        assert_eq!(effects, vec![Effect::Beep]);
    }

    /// #1564: in Visual mode `g;`/`g,` fail before the change list is looked at (nvim's `checkclearopq`):
    /// a beep with no message, even on an empty list, and the selection stays.
    #[test]
    fn visual_change_list_steps_fail_silently() {
        let mut st = EditorState::new(b"abc".to_vec());
        apply_command(
            &mut st,
            &Command::EnterVisual {
                kind: SelectKind::Charwise,
            },
        );
        for cmd in [Command::GotoOlderChange, Command::GotoNewerChange] {
            let effects = apply_command(&mut st, &cmd);
            assert_eq!(effects, vec![Effect::Beep], "{cmd:?}");
            assert_eq!(
                st.view.mode,
                Mode::Visual {
                    kind: SelectKind::Charwise
                }
            );
        }
    }
}
