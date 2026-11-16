//! The Ex line commands (`:d`, `:y`, `:j`, `:>`, `:m`, `:t`, `:sort`, `:s`).

use super::*;
use crate::editor::{LineAddr, SubFlags, SubRange};

/// `:[range]d` deletes whole lines as one undo group; the no-trailing-newline last line and the
/// whole-buffer case leave no dangling blank line.
#[test]
fn delete_lines_over_a_range() {
    let mut w = Workspace::new(b"one\ntwo\nthree\nfour\n".to_vec());
    assert_eq!(
        w.delete_lines(SubRange::Lines(2, 3)),
        2,
        ":2,3d deletes 2 lines"
    );
    assert_eq!(w.focused().doc.bytes(), b"one\nfour\n");
    // One undo restores all of it (single group).
    w.apply(&Command::Undo);
    assert_eq!(w.focused().doc.bytes(), b"one\ntwo\nthree\nfour\n");

    // No range → the cursor's line (like `dd`).
    let mut w = Workspace::new(b"a\nb\nc\n".to_vec());
    assert_eq!(w.delete_lines(SubRange::CurrentLine), 1);
    assert_eq!(w.focused().doc.bytes(), b"b\nc\n");

    // Deleting through an UNTERMINATED last line leaves no dangling blank line.
    let mut w = Workspace::new(b"a\nb".to_vec());
    assert_eq!(w.delete_lines(SubRange::Lines(2, 2)), 1);
    assert_eq!(w.focused().doc.bytes(), b"a");

    // Whole buffer.
    let mut w = Workspace::new(b"x\ny\n".to_vec());
    w.delete_lines(SubRange::WholeFile);
    assert_eq!(w.focused().doc.bytes(), b"");
}

/// `:[range]y` yanks whole lines LINEWISE (non-destructive), so a following `p` opens them below.
#[test]
fn yank_lines_is_linewise_and_non_destructive() {
    let mut w = Workspace::new(b"one\ntwo\nthree\n".to_vec());
    assert_eq!(
        w.yank_lines(SubRange::Lines(1, 2)),
        2,
        ":1,2y yanks two lines"
    );
    // The buffer is untouched by a yank.
    assert_eq!(w.focused().doc.bytes(), b"one\ntwo\nthree\n");
    // `p` (paste after) opens the yanked block below the cursor line (linewise).
    w.apply(&Command::Paste {
        after: true,
        count: 1,
        move_after: false,
    });
    assert_eq!(w.focused().doc.bytes(), b"one\none\ntwo\ntwo\nthree\n");

    // An unterminated last line still yanks a clean linewise block.
    let mut w = Workspace::new(b"a\nb".to_vec());
    assert_eq!(w.yank_lines(SubRange::Lines(2, 2)), 1);
    w.apply(&Command::Paste {
        after: true,
        count: 1,
        move_after: false,
    });
    assert_eq!(w.focused().doc.bytes(), b"a\nb\nb");
}

/// `:[range]j[oin][!]` joins the range's lines, reusing the `J`/`gJ` core join. Ground truth captured
/// from nvim v0.12.4 (`nvim -u NONE`).
#[test]
fn join_lines_matches_vim() {
    // `:j` (no range) joins the current line with the next on a single space, as one undo group.
    let mut w = Workspace::new(b"foo\nbar\nbaz\n".to_vec());
    assert_eq!(w.join_lines(SubRange::CurrentLine, false), 2);
    assert_eq!(w.focused().doc.bytes(), b"foo bar\nbaz\n");
    // Unlike normal `J` (cursor on the join seam), `:j` leaves the cursor on the joined line's first
    // non-blank (`ex_join` → `beginline`; re-probed on nvim v0.12.6, parity `ex_join_cursor_first_non_blank`).
    assert_eq!(w.focused().view.cursor().0, 0);
    // One undo restores both lines (single group).
    w.apply(&Command::Undo);
    assert_eq!(w.focused().doc.bytes(), b"foo\nbar\nbaz\n");

    // `:j!` (bang) raw-concatenates without adjusting whitespace, like `gJ`.
    let mut w = Workspace::new(b"foo\nbar\nbaz\n".to_vec());
    assert_eq!(w.join_lines(SubRange::CurrentLine, true), 2);
    assert_eq!(w.focused().doc.bytes(), b"foobar\nbaz\n");

    // `:2,4j` collapses the three-line range into one, single spaces between.
    let mut w = Workspace::new(b"a\nb\nc\nd\ne\n".to_vec());
    assert_eq!(w.join_lines(SubRange::Lines(2, 4), false), 3);
    assert_eq!(w.focused().doc.bytes(), b"a\nb c d\ne\n");

    // No space is inserted before a `)` that opens the next line.
    let mut w = Workspace::new(b"foo\n)bar\n".to_vec());
    w.join_lines(SubRange::CurrentLine, false);
    assert_eq!(w.focused().doc.bytes(), b"foo)bar\n");

    // The next line's leading whitespace is stripped and collapsed to the single separating space.
    let mut w = Workspace::new(b"foo\n    bar\n".to_vec());
    w.join_lines(SubRange::CurrentLine, false);
    assert_eq!(w.focused().doc.bytes(), b"foo bar\n");

    // A first line ALREADY ending in whitespace keeps it and adds no extra space (Vim `J`).
    let mut w = Workspace::new(b"foo   \nbar\n".to_vec());
    w.join_lines(SubRange::CurrentLine, false);
    assert_eq!(w.focused().doc.bytes(), b"foo   bar\n");

    // A single-line range joins that line with the NEXT one (Vim: a join needs two lines).
    let mut w = Workspace::new(b"a\nb\nc\nd\n".to_vec());
    assert_eq!(w.join_lines(SubRange::Lines(2, 2), false), 2);
    assert_eq!(w.focused().doc.bytes(), b"a\nb c\nd\n");
}

/// `:[range]>` / `:[range]<` shifts the range's lines one indent level per level, reusing the core
/// `>>`/`<<` shift. Ground truth captured from nvim v0.12.4 (`nvim -l`, `set sw=2 et`).
#[test]
fn shift_lines_matches_vim() {
    use crate::editor::EditorOption;

    // `:>` (no range) shifts the current line right one level; the cursor lands on its first non-blank.
    // nvim: `:>` on line 1 of "aa\n…" (sw=2 et) → "  aa", cursor {1, 2}.
    let mut w = Workspace::new(b"aa\nbb\ncc\ndd\nee\n".to_vec());
    w.set_option(EditorOption::ShiftWidth(2));
    assert_eq!(w.shift_lines(SubRange::CurrentLine, false, 1), 1);
    assert_eq!(w.focused().doc.bytes(), b"  aa\nbb\ncc\ndd\nee\n");
    assert_eq!(
        w.focused().view.cursor().0,
        2,
        "first non-blank of the line"
    );
    // One undo restores it (single group).
    w.apply(&Command::Undo);
    assert_eq!(w.focused().doc.bytes(), b"aa\nbb\ncc\ndd\nee\n");

    // `:2,4>` shifts the range; the cursor lands on the first non-blank of the range's LAST line.
    // nvim: cursor {4, 2}. Line 4 ("  dd") starts at byte 13, its first non-blank is byte 15.
    let mut w = Workspace::new(b"aa\nbb\ncc\ndd\nee\n".to_vec());
    w.set_option(EditorOption::ShiftWidth(2));
    assert_eq!(w.shift_lines(SubRange::Lines(2, 4), false, 1), 3);
    assert_eq!(w.focused().doc.bytes(), b"aa\n  bb\n  cc\n  dd\nee\n");
    assert_eq!(
        w.focused().view.cursor().0,
        15,
        "first non-blank of the last line"
    );

    // `:>>>` shifts three levels (Vim's repeated verb chars) as ONE undo group. nvim: "      aa".
    let mut w = Workspace::new(b"aa\nbb\ncc\ndd\nee\n".to_vec());
    w.set_option(EditorOption::ShiftWidth(2));
    assert_eq!(w.shift_lines(SubRange::CurrentLine, false, 3), 1);
    assert_eq!(w.focused().doc.bytes(), b"      aa\nbb\ncc\ndd\nee\n");
    assert_eq!(w.focused().view.cursor().0, 6);
    // A single undo reverses all three levels — the levels merge into one group.
    w.apply(&Command::Undo);
    assert_eq!(
        w.focused().doc.bytes(),
        b"aa\nbb\ncc\ndd\nee\n",
        "one undo group"
    );

    // `:1,2<` dedents one level; the cursor lands on the last line's first non-blank. nvim: cursor {2, 2}.
    let mut w = Workspace::new(b"    aa\n    bb\ncc\n".to_vec());
    w.set_option(EditorOption::ShiftWidth(2));
    assert_eq!(w.shift_lines(SubRange::Lines(1, 2), true, 1), 2);
    assert_eq!(w.focused().doc.bytes(), b"  aa\n  bb\ncc\n");
    assert_eq!(
        w.focused().view.cursor().0,
        7,
        "line 2 '  bb' first non-blank"
    );

    // `:>` indents a whitespace-only line but leaves a truly EMPTY line untouched. nvim `:1,4>` on
    // "aa\n\n  \nbb" → "  aa", "", "    ", "  bb".
    let mut w = Workspace::new(b"aa\n\n  \nbb\n".to_vec());
    w.set_option(EditorOption::ShiftWidth(2));
    w.shift_lines(SubRange::Lines(1, 4), false, 1);
    assert_eq!(w.focused().doc.bytes(), b"  aa\n\n    \n  bb\n");

    // At the default config (shiftwidth 4, spaces) `:>` inserts four spaces.
    let mut w = Workspace::new(b"aa\nbb\n".to_vec());
    assert_eq!(w.shift_lines(SubRange::CurrentLine, false, 1), 1);
    assert_eq!(w.focused().doc.bytes(), b"    aa\nbb\n");

    // `expandtab` off (tab style) indents with a literal tab. nvim `:>` (noet) → "\taa".
    let mut w = Workspace::new(b"aa\nbb\n".to_vec());
    w.set_option(EditorOption::ExpandTab(false));
    w.shift_lines(SubRange::CurrentLine, false, 1);
    assert_eq!(w.focused().doc.bytes(), b"\taa\nbb\n");

    // `:<` never dedents past column 0 (Vim clamps).
    let mut w = Workspace::new(b"aa\nbb\n".to_vec());
    w.set_option(EditorOption::ShiftWidth(2));
    w.shift_lines(SubRange::CurrentLine, true, 1);
    assert_eq!(w.focused().doc.bytes(), b"aa\nbb\n");
}

/// `:[range]m {addr}` moves whole lines to after the destination as one undo group; a destination
/// inside the source is declined (E134).
#[test]
fn move_lines_relocates_the_span() {
    let mut w = Workspace::new(b"one\ntwo\nthree\nfour\n".to_vec());
    assert_eq!(
        w.move_lines(SubRange::Lines(1, 2), LineAddr::Line(4)),
        Some(2)
    );
    assert_eq!(w.focused().doc.bytes(), b"three\nfour\none\ntwo\n");
    w.apply(&Command::Undo);
    assert_eq!(
        w.focused().doc.bytes(),
        b"one\ntwo\nthree\nfour\n",
        "one undo group"
    );

    // `:3,4m0` → to the top.
    let mut w = Workspace::new(b"one\ntwo\nthree\nfour\n".to_vec());
    assert_eq!(
        w.move_lines(SubRange::Lines(3, 4), LineAddr::Line(0)),
        Some(2)
    );
    assert_eq!(w.focused().doc.bytes(), b"three\nfour\none\ntwo\n");

    // `:m$` on the cursor's line (line 1) → to the end.
    let mut w = Workspace::new(b"a\nb\nc\n".to_vec());
    assert_eq!(w.move_lines(SubRange::CurrentLine, LineAddr::Last), Some(1));
    assert_eq!(w.focused().doc.bytes(), b"b\nc\na\n");

    // The moved lines take their marks along; the lines they pass shift (nvim `ex_copymove`, #1125).
    let mut w = Workspace::new(b"a\nbxy\nc\nd\n".to_vec());
    w.set_mark_on_line(SubRange::Lines(2, 2), 'a');
    w.set_mark_on_line(SubRange::Lines(3, 3), 'c');
    assert_eq!(
        w.move_lines(SubRange::Lines(2, 2), LineAddr::Line(3)),
        Some(1)
    );
    assert_eq!(w.focused().doc.bytes(), b"a\nc\nbxy\nd\n");
    assert_eq!(w.focused().view.named_mark('a'), Some(BytePos(4)));
    assert_eq!(w.focused().view.named_mark('c'), Some(BytePos(2)));

    // Destination inside the source → declined (Vim E134).
    let mut w = Workspace::new(b"a\nb\nc\nd\n".to_vec());
    assert_eq!(w.move_lines(SubRange::Lines(2, 3), LineAddr::Line(2)), None);
    assert_eq!(
        w.focused().doc.bytes(),
        b"a\nb\nc\nd\n",
        "declined move leaves the buffer"
    );
}

/// `:[range]t {addr}` copies whole lines to after the destination, leaving the source in place.
#[test]
fn copy_lines_duplicates_the_span() {
    let mut w = Workspace::new(b"a\nb\nc\n".to_vec());
    assert_eq!(
        w.copy_lines(SubRange::Lines(1, 1), LineAddr::Line(2)),
        Some(1)
    );
    assert_eq!(w.focused().doc.bytes(), b"a\nb\na\nc\n");

    // `:1,2t0` copies the block to the top.
    let mut w = Workspace::new(b"a\nb\nc\n".to_vec());
    assert_eq!(
        w.copy_lines(SubRange::Lines(1, 2), LineAddr::Line(0)),
        Some(2)
    );
    assert_eq!(w.focused().doc.bytes(), b"a\nb\na\nb\nc\n");
}

/// `:[range]sort` sorts lines lexicographically or numerically, with `!` reverse and `u` unique, as
/// one undo group; a range limits it to those lines.
#[test]
fn sort_lines_variants() {
    use crate::SortOptions;
    let opts = |reverse, numeric: bool, unique| SortOptions {
        reverse,
        key: if numeric {
            crate::SortKind::Decimal
        } else {
            crate::SortKind::Text
        },
        unique,
        ..SortOptions::default()
    };
    let mut w = Workspace::new(b"banana\napple\ncherry\n".to_vec());
    w.sort_lines(SubRange::WholeFile, &opts(false, false, false))
        .unwrap();
    assert_eq!(w.focused().doc.bytes(), b"apple\nbanana\ncherry\n");

    // numeric: 10 sorts after 2, not lexicographically before it.
    let mut w = Workspace::new(b"10\n2\n1\n".to_vec());
    w.sort_lines(SubRange::WholeFile, &opts(false, true, false))
        .unwrap();
    assert_eq!(w.focused().doc.bytes(), b"1\n2\n10\n");

    // reverse (descending).
    let mut w = Workspace::new(b"a\nb\nc\n".to_vec());
    w.sort_lines(SubRange::WholeFile, &opts(true, false, false))
        .unwrap();
    assert_eq!(w.focused().doc.bytes(), b"c\nb\na\n");

    // unique drops duplicate lines after sorting.
    let mut w = Workspace::new(b"b\na\nb\na\n".to_vec());
    assert_eq!(
        w.sort_lines(SubRange::WholeFile, &opts(false, false, true)),
        Ok(2),
        "2 dupes removed"
    );
    assert_eq!(w.focused().doc.bytes(), b"a\nb\n");

    // A range limits the sort to those lines; the rest stays put.
    let mut w = Workspace::new(b"z\n3\n1\n2\n".to_vec());
    w.sort_lines(SubRange::Lines(2, 4), &opts(false, true, false))
        .unwrap();
    assert_eq!(w.focused().doc.bytes(), b"z\n1\n2\n3\n");
}

/// #719 (nvim v0.12.6): a key with no number sorts before every number, a number too large saturates,
/// `u` compares whole lines in the output order (after `!`), and a bad pattern leaves the text alone.
#[test]
fn sort_lines_numeric_keys_unique_and_bad_patterns() {
    use crate::SortOptions;
    let sorted = |text: &str, opts: SortOptions| {
        let mut w = Workspace::new(text.as_bytes().to_vec());
        w.sort_lines(SubRange::WholeFile, &opts).unwrap();
        String::from_utf8(w.focused().doc.bytes().to_vec()).unwrap()
    };
    let numeric = SortOptions {
        key: crate::SortKind::Decimal,
        ..SortOptions::default()
    };
    assert_eq!(sorted("b\n-5\na\n3\n", numeric.clone()), "b\na\n-5\n3\n");
    assert_eq!(
        sorted(
            "99999999999999999999\n5\n-99999999999999999999\n",
            numeric.clone()
        ),
        "-99999999999999999999\n5\n99999999999999999999\n"
    );
    let numeric_unique = SortOptions {
        unique: true,
        ..numeric
    };
    assert_eq!(sorted("1\n01\n2\n", numeric_unique), "1\n01\n2\n");
    let ic_unique = SortOptions {
        ignore_case: true,
        unique: true,
        ..SortOptions::default()
    };
    assert_eq!(sorted("a\nA\nb\n", ic_unique.clone()), "a\nb\n");
    let reversed = SortOptions {
        reverse: true,
        ..ic_unique
    };
    assert_eq!(sorted("a\nA\nb\n", reversed), "b\nA\n");

    let mut w = Workspace::new(b"b\na\n".to_vec());
    let bad = SortOptions {
        pattern: Some("\\(".into()),
        ..SortOptions::default()
    };
    assert!(w.sort_lines(SubRange::WholeFile, &bad).is_err());
    assert_eq!(w.focused().doc.bytes(), b"b\na\n");
}

/// #1104: `:sort x`/`o`/`b`/`f`/`l` key extraction, checked against nvim v0.12.6.
#[test]
fn sort_lines_number_formats_and_locale() {
    use crate::{SortKind, SortOptions};
    let sorted = |text: &str, key: SortKind| {
        let mut w = Workspace::new(text.as_bytes().to_vec());
        let opts = SortOptions {
            key,
            ..SortOptions::default()
        };
        w.sort_lines(SubRange::WholeFile, &opts).unwrap();
        String::from_utf8(w.focused().doc.bytes().to_vec()).unwrap()
    };
    assert_eq!(
        sorted("x0x1F\nb\n0x0a\n-0x3\n0xff\nzz\n", SortKind::Hex),
        "zz\n-0x3\n0x0a\nb\nx0x1F\n0xff\n"
    );
    assert_eq!(
        sorted("0b101\n0b11\nb\n-0b1\n10\n", SortKind::Binary),
        "b\n-0b1\n10\n0b11\n0b101\n"
    );
    assert_eq!(
        sorted("017\n8\n07\na\n-01\n9\n", SortKind::Octal),
        "a\n-01\n8\n9\n07\n017\n"
    );
    assert_eq!(
        sorted("1.5\na\n-2e1\n0.3\nb\n1e-1\n3\n", SortKind::Float),
        "-2e1\na\nb\n1e-1\n0.3\n1.5\n3\n"
    );
    assert_eq!(
        sorted(
            "x\n1.5e\ninf\n-inf\n2\n \n0x1p2\n0x.8\n+ 3\n",
            SortKind::Float
        ),
        "-inf\n \nx\n0x.8\n1.5e\n2\n+ 3\n0x1p2\ninf\n"
    );
    assert_eq!(sorted("B\na\nb\nA\n", SortKind::Locale), "a\nA\nb\nB\n");
}

#[test]
fn sort_lines_ignore_case_and_pattern() {
    use crate::SortOptions;
    // `i` — case-insensitive: "Banana" sorts with "apple"/"cherry", not before all lowercase.
    let mut w = Workspace::new(b"cherry\nBanana\napple\n".to_vec());
    w.sort_lines(
        SubRange::WholeFile,
        &SortOptions {
            ignore_case: true,
            ..SortOptions::default()
        },
    )
    .unwrap();
    assert_eq!(w.focused().doc.bytes(), b"apple\nBanana\ncherry\n");

    // `/pattern/` — sort on the text AFTER the match (here, after the leading `id=` tag).
    let mut w = Workspace::new(b"id=charlie\nid=alice\nid=bob\n".to_vec());
    w.sort_lines(
        SubRange::WholeFile,
        &SortOptions {
            pattern: Some("id=".into()),
            ..SortOptions::default()
        },
    )
    .unwrap();
    assert_eq!(w.focused().doc.bytes(), b"id=alice\nid=bob\nid=charlie\n");

    // `/pattern/` + `r` — sort on the MATCHED text itself (the number), numerically.
    let mut w = Workspace::new(b"a99\nb100\nc9\n".to_vec());
    w.sort_lines(
        SubRange::WholeFile,
        &SortOptions {
            pattern: Some("\\d\\+".into()),
            use_match: true,
            key: crate::SortKind::Decimal,
            ..SortOptions::default()
        },
    )
    .unwrap();
    assert_eq!(w.focused().doc.bytes(), b"c9\na99\nb100\n");
}

/// F-009: a whole-file substitute (what `g&` repeats) applies across every line, honouring the flags.
#[test]
fn substitute_whole_file_is_what_g_ampersand_repeats() {
    let mut w = Workspace::new(b"x x\nx x\n".to_vec());
    // `g&` reuses the last flags — here global (all matches on every line).
    let out = w
        .substitute(
            SubRange::WholeFile,
            "x",
            "y",
            SubFlags {
                global: true,
                ignore_case: None,
            },
        )
        .unwrap();
    assert_eq!(w.focused().doc.bytes(), b"y y\ny y\n");
    assert_eq!(out.replacements, 4);
}

/// F-009: a current-line substitute (what `&` repeats) acts only on the cursor's line, first match by
/// default — running it again after moving the cursor repeats on the new line.
#[test]
fn substitute_current_line_repeats_on_the_cursors_line() {
    let mut w = Workspace::new(b"x x\nx x\n".to_vec());
    w.substitute(SubRange::CurrentLine, "x", "y", SubFlags::default())
        .unwrap();
    assert_eq!(w.focused().doc.bytes(), b"y x\nx x\n", "first x on line 1");
    w.place_focused_cursor(BytePos(4)); // start of line 2
    w.substitute(SubRange::CurrentLine, "x", "y", SubFlags::default())
        .unwrap();
    assert_eq!(w.focused().doc.bytes(), b"y x\ny x\n", "repeat on line 2");
}

/// F-009: `:s///n` (report-only) counts matches WITHOUT editing the buffer or adding an undo entry.
/// Numbers verified vs nvim v0.12.4: `%s/foo//gn` on this buffer → "6 matches on 3 lines".
#[test]
fn substitute_count_reports_without_editing_or_undo() {
    let src = b"foo bar foo\nfoo baz\nno match here\nfoo foo foo\n";
    let mut w = Workspace::new(src.to_vec());
    w.place_focused_cursor(BytePos(2)); // anywhere — count must not move the cursor
    let global = SubFlags {
        global: true,
        ignore_case: None,
    };
    let out = w
        .substitute_count(SubRange::WholeFile, "foo", global)
        .unwrap();
    assert_eq!(
        out.replacements, 6,
        "6 matches on lines 1/2/4 (nvim: 6 matches)"
    );
    assert_eq!(out.lines, 3, "3 distinct lines had a match");
    // The buffer, cursor, and dirty flag are all untouched — no edit, no undo entry.
    assert_eq!(
        w.focused().doc.bytes(),
        src,
        "count-only must not edit the buffer"
    );
    assert_eq!(
        w.focused().view.cursor().0,
        2,
        "count-only must not move the cursor"
    );
    assert!(
        !w.focused().doc.is_modified(),
        "count-only must not dirty the buffer"
    );
    // An undo is a genuine no-op: there was no transaction to undo.
    w.apply(&Command::Undo);
    assert_eq!(
        w.focused().doc.bytes(),
        src,
        "no undo entry was created by :s///n"
    );
}

/// F-009: without `g`, `:s///n` counts one match per line (verified vs nvim: "3 matches on 3 lines");
/// a single-line range counts only that line ("2 matches on 1 line").
#[test]
fn substitute_count_honors_g_flag_and_range() {
    let mut w = Workspace::new(b"foo bar foo\nfoo baz\nno match here\nfoo foo foo\n".to_vec());
    // No `g`: one match per line, three lines match.
    let out = w
        .substitute_count(SubRange::WholeFile, "foo", SubFlags::default())
        .unwrap();
    assert_eq!(
        (out.replacements, out.lines),
        (3, 3),
        "nvim: 3 matches on 3 lines"
    );
    // Range = line 1 only, with `g`: both `foo`s on that line.
    let out = w
        .substitute_count(
            SubRange::Lines(1, 1),
            "foo",
            SubFlags {
                global: true,
                ignore_case: None,
            },
        )
        .unwrap();
    assert_eq!(
        (out.replacements, out.lines),
        (2, 1),
        "nvim: 2 matches on 1 line"
    );
}
