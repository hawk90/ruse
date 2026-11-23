//! Search. Split from `editor/tests.rs` (#1397).

use crate::command::SearchOp;
use crate::editor::*;

fn run(initial: &str, cmds: &[Command]) -> EditorState {
    let mut st = EditorState::new(initial.as_bytes().to_vec());
    for c in cmds {
        apply_command(&mut st, c);
    }
    st
}

fn search(pattern: &str) -> Command {
    Command::Search {
        op: SearchOp::Move,
        count: 1,
        pattern: pattern.to_string(),
        backward: false,
        offset: SearchOffset::None,
    }
}

/// `$` — rest on the last char of the line (the start position for the backward-search tests).
fn eol() -> Command {
    Command::Move(1, Motion::LineEnd)
}

/// `[count]?pat` — a backward search motion (`SearchOp::Move`).
fn search_back(count: u32, pattern: &str) -> Command {
    Command::Search {
        op: SearchOp::Move,
        count,
        pattern: pattern.to_string(),
        backward: true,
        offset: SearchOffset::None,
    }
}

/// #692 review: `^` / `$` are line anchors in a whole-buffer search, but the empty slot after the
/// buffer's final "\n" is not a line. nvim: `/^$` on "abc\ndef\n" is E486 (cursor stays); `/^` + `n`
/// wraps 4 → 0 → 4; on "abc\n\ndef\n" `/^$` + `n` stays on the one empty line (4, 4).
#[test]
fn line_anchors_never_match_past_the_final_newline() {
    // (`SearchNext` goes through `search_fwd`, over the same `search_spans` as the `/pat` motion.)
    let next = |p: &str| Command::SearchNext(p.into());
    let st = run("abc\ndef\n", &[next("^$")]);
    assert_eq!(st.cursor().0, 0, "no empty line: the search fails");
    let st = run("abc\ndef\n", &[next("^$"), next("^$")]);
    assert_eq!(st.cursor().0, 0);

    let st = run("abc\ndef\n", &[next("^")]);
    assert_eq!(st.cursor().0, 4);
    let st = run("abc\ndef\n", &[next("^"), next("^")]);
    assert_eq!(st.cursor().0, 0, "wraps instead of landing past the end");
    let st = run("abc\ndef\n", &[next("^"), next("^"), next("^")]);
    assert_eq!(st.cursor().0, 4);

    let st = run("abc\n\ndef\n", &[next("^$")]);
    assert_eq!(st.cursor().0, 4);
    let st = run("abc\n\ndef\n", &[next("^$"), next("^$")]);
    assert_eq!(st.cursor().0, 4);

    for pat in ["$", r"\_$", "x*"] {
        let st = run("abc\ndef\n", &[next(pat), next(pat), next(pat), next(pat)]);
        assert!(
            st.cursor().0 < 8,
            "{pat}: cursor {} is past the last line",
            st.cursor().0
        );
    }
}

/// F-009 #3: `/pat` is a Vim-REGEX search now (not literal). `a\+` matches one-or-more `a`; the
/// cursor lands on the start of the next match.
#[test]
fn slash_search_uses_the_vim_regex_engine() {
    // `a\+` is a Vim-regex quantifier (magic `\+`), not the literal "a\+".
    let st = run("x aa bbb aaa", &[search("a\\+")]);
    // From offset 0, the next match after cur+1 is the "aa" at offset 2.
    assert_eq!(st.cursor().0, 2);
    // `n` (SearchNext) steps to the following, non-overlapping match ("cc" at offset 9).
    let st = run(
        "x aa bbb cc",
        &[search("cc"), Command::SearchNext("cc".into())],
    );
    // Single `cc` match at 9; `n` wraps back to it.
    assert_eq!(st.cursor().0, 9);
}

/// F-009 #3: `\zs` moves the reported match START — `/foo\zsbar` lands the cursor on `bar`, but
/// only where `bar` follows `foo`.
#[test]
fn zs_lands_the_cursor_at_the_reset_start() {
    let st = run("a foobar foo bar", &[search("foo\\zsbar")]);
    assert_eq!(st.cursor().0, 5); // the 'b' of the bar inside "foobar"
}

/// F-009 #1: search is CASE-SENSITIVE by default (Vim's factory setting, what the oracle runs), so
/// `/Foo` does not match `foo`.
#[test]
fn search_is_case_sensitive_by_default() {
    let st = run("a foo b", &[search("Foo")]);
    assert_eq!(st.cursor().0, 0, "no match → cursor unmoved");
}

/// F-009 #1: with `smartcase` on, an all-lowercase pattern matches case-insensitively, but an
/// uppercase in the pattern forces a case-sensitive match (case-smart search).
#[test]
fn smartcase_makes_search_case_smart() {
    // lowercase pattern → case-insensitive → matches "FOO"
    let mut st = EditorState::new(b"x FOO y".to_vec());
    st.set_search_case(true, true);
    apply_command(&mut st, &search("foo"));
    assert_eq!(st.cursor().0, 2);
    // uppercase in the pattern → case-sensitive → "Foo" does NOT match "foo"
    let mut st = EditorState::new(b"x foo y".to_vec());
    st.set_search_case(true, true);
    apply_command(&mut st, &search("Foo"));
    assert_eq!(
        st.cursor().0,
        0,
        "smartcase forced case-sensitive → no match"
    );
}

/// F-009: `d/pat` deletes `[cursor, match)` — the search operand still composes with operators.
#[test]
fn delete_to_search_match() {
    let st = run(
        "abcXYZdef",
        &[Command::Search {
            op: SearchOp::Delete,
            count: 1,
            pattern: "XYZ".into(),
            backward: false,
            offset: SearchOffset::None,
        }],
    );
    assert_eq!(st.as_str().unwrap(), "XYZdef");
}

fn word_under(forward: bool, op: SearchOp, count: u32) -> Command {
    Command::SearchWordUnder {
        forward,
        whole_word: true,
        op,
        count,
    }
}

/// #1071: `*`/`#` search from the keyword's START (Vim moves there first), so `*` before a keyword skips
/// that keyword and `#` mid-word skips the word itself; under an operator the span starts at the cursor.
/// Values from nvim 0.12.6.
#[test]
fn star_and_hash_search_from_the_keyword_start() {
    // `*` on the leading blanks of "  foo bar foo" lands on the SECOND foo (10), not the first (2).
    let st = run("  foo bar foo\n", &[word_under(true, SearchOp::Move, 1)]);
    assert_eq!(st.cursor().0, 10);
    // `ll#` on "foo bar foo": from the word start (0) backward wraps to the last foo (8).
    let mut st = EditorState::new(b"foo bar foo\n".to_vec());
    st.set_cursor(BytePos(2));
    apply_command(&mut st, &word_under(false, SearchOp::Move, 1));
    assert_eq!(st.cursor().0, 8);
    // `ld*` deletes from the cursor (1), not the word start, up to the next foo.
    let mut st = EditorState::new(b"foo bar foo baz\n".to_vec());
    st.set_cursor(BytePos(1));
    apply_command(&mut st, &word_under(true, SearchOp::Delete, 1));
    assert_eq!(st.as_str().unwrap(), "ffoo baz\n");
    // `2*` steps two matches.
    let st = run("foo bar foo foo\n", &[word_under(true, SearchOp::Move, 2)]);
    assert_eq!(st.cursor().0, 12);
    // No keyword on the line: nothing happens (the frontend reports E348).
    let st = run("   \nfoo\n", &[word_under(true, SearchOp::Delete, 1)]);
    assert_eq!(st.as_str().unwrap(), "   \nfoo\n");
}

/// #800: the `!` filter numbers its `:{range}!` from the span the operator would cover.
#[test]
fn span_lines_measures_search_mark_and_gn_spans() {
    let probe = |pattern: &str| Command::Search {
        op: SearchOp::Yank,
        count: 1,
        pattern: pattern.into(),
        backward: false,
        offset: SearchOffset::None,
    };
    let st = EditorState::new(b"a\nb\nx\n".to_vec());
    // `!/x` from line 1: the exclusive-linewise rule drops line 3 (`:.,.+1!`).
    assert_eq!(st.span_lines(&probe("x")), Some((0, 1)));
    // A mid-line match keeps its line (`:.,$!`).
    let st = EditorState::new(b"a\nb\nzx\n".to_vec());
    assert_eq!(st.span_lines(&probe("x")), Some((0, 2)));
    // No match: no range (nvim E486, no command line).
    assert_eq!(st.span_lines(&probe("qqq")), None);
    // A lone keyword: the empty span covers the cursor's line (`:.!`).
    let st = EditorState::new(b"foo bar\nzz\n".to_vec());
    assert_eq!(
        st.span_lines(&word_under(true, SearchOp::Yank, 1)),
        Some((0, 0))
    );
    // `gn` covers the match's lines.
    let st = EditorState::new(b"foo\nbar\nbaz\n".to_vec());
    let gn = Command::SearchObject {
        op: SearchOp::Yank,
        count: 1,
        pattern: "baz".into(),
        backward: false,
    };
    assert_eq!(st.span_lines(&gn), Some((2, 2)));
    // A non-span command has no span.
    assert_eq!(st.span_lines(&Command::MoveDown), None);
}

/// #1071: after the exclusive-linewise rule adjusted the end, `gq` lands on the line BELOW the
/// formatted block (nvim `op_format`); `gw` keeps the cursor. Values from nvim 0.12.6.
#[test]
fn gq_over_an_adjusted_search_span_lands_below_the_block() {
    let gq = |keep: bool| Command::Search {
        op: SearchOp::Format { keep },
        count: 1,
        pattern: "end".into(),
        backward: false,
        offset: SearchOffset::None,
    };
    // `  end` is indented, so the match is mid-line: no adjustment, the whole span is joined.
    let st = run("aa bb\ncc\n  end\nzz\n", &[gq(false)]);
    assert_eq!(st.as_str().unwrap(), "aa bb cc end\nzz\n");
    assert_eq!(st.cursor().0, 0);
    let st = run("aa bb\ncc\nend\nzz\n", &[gq(false)]);
    assert_eq!(st.as_str().unwrap(), "aa bb cc\nend\nzz\n");
    assert_eq!(
        st.cursor().0,
        9,
        "first non-blank of the line below the block"
    );
    let st = run("aa bb\ncc\nend\nzz\n", &[gq(true)]);
    assert_eq!(st.cursor().0, 0, "gw restores the cursor");
}

/// F-009: `?pat` lands the cursor on the match STRICTLY BEFORE the cursor. "foo" is at 0 and 8;
/// from EOL the backward search stops on 8. Verified against nvim v0.12.4.
#[test]
fn backward_search_lands_on_previous_match() {
    let st = run("foo bar foo baz", &[eol(), search_back(1, "foo")]);
    assert_eq!(st.cursor().0, 8);
}

/// F-009: `?pat` wraps to the LAST match when none starts before the cursor (cursor at BOL).
#[test]
fn backward_search_wraps_to_last_match() {
    let st = run("bar foo baz", &[search_back(1, "foo")]);
    assert_eq!(st.cursor().0, 4);
}

/// F-009: `[count]?pat` jumps to the count-th previous match. "foo" at 0/6/12; from EOL, `2?foo`
/// steps back over 12 to 6.
#[test]
fn backward_search_honours_count() {
    let st = run("foo x foo x foo x end", &[eol(), search_back(2, "foo")]);
    assert_eq!(st.cursor().0, 6);
}

/// F-009: `n`/`N` are direction-RELATIVE. After a backward search, `SearchPrev` (what `n` maps to)
/// continues backward and `SearchNext` (what `N` maps to) reverses to forward. The frontend chooses
/// which primitive to emit from the stored direction; here we assert the primitives themselves.
#[test]
fn direction_relative_repeat_after_backward_search() {
    // "foo" at 0/6/12. From EOL, `?foo` -> 12, then backward-repeat -> 6.
    let st = run(
        "foo x foo x foo",
        &[
            eol(),
            search_back(1, "foo"),
            Command::SearchPrev("foo".into()),
        ],
    );
    assert_eq!(st.cursor().0, 6, "n after ? continues backward");
    // `?foo` -> 12, then forward-repeat (N reverses) wraps past EOL to 0.
    let st = run(
        "foo x foo x foo",
        &[
            eol(),
            search_back(1, "foo"),
            Command::SearchNext("foo".into()),
        ],
    );
    assert_eq!(st.cursor().0, 0, "N after ? reverses to forward (wraps)");
}

/// F-009: `d?pat` is an EXCLUSIVE charwise motion over `[match, cursor)` — the char under the cursor
/// is NOT removed. "foo bar baz qux", cursor at EOL (the 'x'), `d?bar` deletes indices 4..14
/// ("bar baz qu"), leaving "foo x". Geometry verified against nvim v0.12.4.
#[test]
fn delete_backward_search_is_exclusive() {
    let st = run(
        "foo bar baz qux",
        &[
            eol(),
            Command::Search {
                op: SearchOp::Delete,
                count: 1,
                pattern: "bar".into(),
                backward: true,
                offset: SearchOffset::None,
            },
        ],
    );
    assert_eq!(st.as_str().unwrap(), "foo x");
    assert_eq!(st.cursor().0, 4);
}

/// F-009: `c?pat` deletes `[match, cursor)` and enters Insert AT the match — it must not inherit the
/// `$` append intent (curswant=MAXCOL) and park the caret at the line end. Regression for that bug.
#[test]
fn change_backward_search_inserts_at_match() {
    let mut st = EditorState::new(b"foo bar baz qux".to_vec());
    apply_command(&mut st, &eol());
    apply_command(
        &mut st,
        &Command::Search {
            op: SearchOp::Change,
            count: 1,
            pattern: "bar".into(),
            backward: true,
            offset: SearchOffset::None,
        },
    );
    assert_eq!(st.as_str().unwrap(), "foo x");
    assert_eq!(
        st.cursor().0,
        4,
        "Insert caret at the match, not the line end"
    );
    apply_command(&mut st, &Command::InsertChar('Z'));
    assert_eq!(st.as_str().unwrap(), "foo Zx");
}

/// F-009: `y?pat` yanks `[match, cursor)` charwise and leaves the cursor at the low end (the match).
#[test]
fn yank_backward_search_charwise() {
    let st = run(
        "foo bar baz qux",
        &[
            eol(),
            Command::Search {
                op: SearchOp::Yank,
                count: 1,
                pattern: "bar".into(),
                backward: true,
                offset: SearchOffset::None,
            },
        ],
    );
    assert_eq!(st.as_str().unwrap(), "foo bar baz qux");
    assert_eq!(st.cursor().0, 4);
    assert_eq!(st.register().text(), b"bar baz qu");
    assert!(!st.register().is_linewise());
}

// --- search offsets (#475) — every assertion is the byte position / buffer nvim v0.12.4 produces
//     for the same keystrokes, captured headless with `nvim -u NONE`. ---
fn soff(op: SearchOp, count: u32, pattern: &str, backward: bool, offset: SearchOffset) -> Command {
    Command::Search {
        op,
        count,
        pattern: pattern.to_string(),
        backward,
        offset,
    }
}

// A 3-line buffer whose byte layout the offset tests reference: "foo" at 12..15 on line 1, "foobar"
// at 39..45 on line 3. Line starts: L1=0, L2=16, L3=33.
const A: &str = "hello world foo\nsecond line here\nthird foobar line";

#[test]
fn search_offset_end_moves_to_match_last_char() {
    // /foo/e -> last char of the match (nvim [1,14]).
    assert_eq!(
        run(
            A,
            &[soff(SearchOp::Move, 1, "foo", false, SearchOffset::End(0))]
        )
        .cursor()
        .0,
        14
    );
    // /foo/e+1 -> one char right, CROSSING to line 2 col 0 (nvim [2,0] = byte 16).
    assert_eq!(
        run(
            A,
            &[soff(SearchOp::Move, 1, "foo", false, SearchOffset::End(1))]
        )
        .cursor()
        .0,
        16
    );
    // /foo/e-1 -> one left of the last char (nvim [1,13]).
    assert_eq!(
        run(
            A,
            &[soff(SearchOp::Move, 1, "foo", false, SearchOffset::End(-1))]
        )
        .cursor()
        .0,
        13
    );
}

#[test]
fn search_offsets_step_whole_grapheme_clusters() {
    // nvim v0.12.6 (#1132): `/ab/s-1` on "x e\u{301}ab z" lands on the `e`, not on its mark.
    let mv = |text: &str, pat: &str, off| {
        run(text, &[soff(SearchOp::Move, 1, pat, false, off)])
            .cursor()
            .0
    };
    assert_eq!(mv("x e\u{301}ab z", "ab", SearchOffset::Start(-1)), 2);
    assert_eq!(mv("x a\u{301}b z", "b", SearchOffset::Start(-1)), 2);
    assert_eq!(mv("x 👍🏽ab z", "ab", SearchOffset::Start(-1)), 2);
    // `/ae\u{301} /e-1`: the match's last char is the space, one cluster back is `e`+mark.
    assert_eq!(mv("x ae\u{301} z", "ae\u{301} ", SearchOffset::End(-1)), 3);
    // Forward over a flag (two code points) and a ZWJ family.
    assert_eq!(mv("x ab🇰🇷y z", "ab", SearchOffset::End(2)), 12);
    assert_eq!(mv("x ab👨‍👩‍👧y z", "ab", SearchOffset::End(2)), 22);
    // `d/ab/e+2` takes the whole `e`+mark.
    let st = run(
        "x ab e\u{301}y z",
        &[soff(SearchOp::Delete, 1, "ab", false, SearchOffset::End(2))],
    );
    assert_eq!(st.doc.bytes(), b"y z");
}

#[test]
fn search_offset_start_moves_relative_to_match_start() {
    // /foo/s == /foo/b -> first char of the match (nvim [1,12]).
    assert_eq!(
        run(
            A,
            &[soff(
                SearchOp::Move,
                1,
                "foo",
                false,
                SearchOffset::Start(0)
            )]
        )
        .cursor()
        .0,
        12
    );
    // /foo/s+2 -> two right of start (nvim [1,14]).
    assert_eq!(
        run(
            A,
            &[soff(
                SearchOp::Move,
                1,
                "foo",
                false,
                SearchOffset::Start(2)
            )]
        )
        .cursor()
        .0,
        14
    );
    // /foo/s-1 -> one left of start (nvim [1,11]).
    assert_eq!(
        run(
            A,
            &[soff(
                SearchOp::Move,
                1,
                "foo",
                false,
                SearchOffset::Start(-1)
            )]
        )
        .cursor()
        .0,
        11
    );
}

#[test]
fn search_offset_line_moves_to_column_zero_of_target_line() {
    // /foobar/+1 -> one line below the match, clamped to the last line, COLUMN 0 (nvim [3,0]=byte 33).
    assert_eq!(
        run(
            A,
            &[soff(
                SearchOp::Move,
                1,
                "foobar",
                false,
                SearchOffset::Line(1)
            )]
        )
        .cursor()
        .0,
        33
    );
    // /foobar/-1 -> one line above (nvim [2,0]=byte 16).
    assert_eq!(
        run(
            A,
            &[soff(
                SearchOp::Move,
                1,
                "foobar",
                false,
                SearchOffset::Line(-1)
            )]
        )
        .cursor()
        .0,
        16
    );
    // /foobar/-2 -> two lines above (nvim [1,0]=byte 0).
    assert_eq!(
        run(
            A,
            &[soff(
                SearchOp::Move,
                1,
                "foobar",
                false,
                SearchOffset::Line(-2)
            )]
        )
        .cursor()
        .0,
        0
    );
}

#[test]
fn delete_search_offset_end_is_inclusive() {
    // d/foo/e -> deletes [cursor, last-char] INCLUSIVE = the whole first line's content (nvim).
    let st = run(
        A,
        &[soff(
            SearchOp::Delete,
            1,
            "foo",
            false,
            SearchOffset::End(0),
        )],
    );
    assert_eq!(
        st.as_str().unwrap(),
        "\nsecond line here\nthird foobar line"
    );
    assert_eq!(st.register().text(), b"hello world foo");
    assert!(!st.register().is_linewise());
    // d/foo/e+1 -> inclusive THROUGH the offset char on line 2 ('s'): removes "hello world foo\ns".
    let st = run(
        A,
        &[soff(
            SearchOp::Delete,
            1,
            "foo",
            false,
            SearchOffset::End(1),
        )],
    );
    assert_eq!(st.as_str().unwrap(), "econd line here\nthird foobar line");
    // d/foo/e-1 -> through the 'o' before the last char: leaves "o" on line 1.
    let st = run(
        A,
        &[soff(
            SearchOp::Delete,
            1,
            "foo",
            false,
            SearchOffset::End(-1),
        )],
    );
    assert_eq!(
        st.as_str().unwrap(),
        "o\nsecond line here\nthird foobar line"
    );
}

#[test]
fn delete_search_offset_start_is_exclusive() {
    // d/foo/s -> deletes [cursor, match_start) EXCLUSIVE = "hello world " (nvim), leaving "foo".
    let st = run(
        A,
        &[soff(
            SearchOp::Delete,
            1,
            "foo",
            false,
            SearchOffset::Start(0),
        )],
    );
    assert_eq!(
        st.as_str().unwrap(),
        "foo\nsecond line here\nthird foobar line"
    );
    assert_eq!(st.register().text(), b"hello world ");
    // d/foo/s+2 -> exclusive up to start+2, leaving "o" (nvim reg "hello world fo").
    let st = run(
        A,
        &[soff(
            SearchOp::Delete,
            1,
            "foo",
            false,
            SearchOffset::Start(2),
        )],
    );
    assert_eq!(
        st.as_str().unwrap(),
        "o\nsecond line here\nthird foobar line"
    );
    assert_eq!(st.register().text(), b"hello world fo");
}

#[test]
fn delete_search_offset_line_is_linewise() {
    // A 5-line buffer: foo on line 2. d/foo/+1 deletes lines 1..3 LINEWISE (nvim).
    let b = "a\nb foo c\nd\ne foo f\ng";
    let st = run(
        b,
        &[soff(
            SearchOp::Delete,
            1,
            "foo",
            false,
            SearchOffset::Line(1),
        )],
    );
    assert_eq!(st.as_str().unwrap(), "e foo f\ng");
    assert_eq!(st.register().text(), b"a\nb foo c\nd\n");
    assert!(st.register().is_linewise());
    assert_eq!(st.cursor().0, 0);
}

#[test]
fn change_search_offset_line_keeps_indent() {
    // c/foo/+0 -> linewise change of the match line, keeping its leading indent, Insert at indent end.
    let mut st = EditorState::new(b"  ab foo cd\nnext".to_vec());
    apply_command(
        &mut st,
        &soff(SearchOp::Change, 1, "foo", false, SearchOffset::Line(0)),
    );
    assert_eq!(
        st.cursor().0,
        2,
        "Insert caret at the indent end (nvim [1,2])"
    );
    assert_eq!(st.mode(), Mode::Insert);
    apply_command(&mut st, &Command::InsertChar('Z'));
    assert_eq!(st.as_str().unwrap(), "  Z\nnext");
}

#[test]
fn search_offset_reapplied_by_repeat_advances_past_current_match() {
    // "abc foo def foo xyz" — foo at 4..7 and 12..15. Re-issuing the same offset search (what `n`
    // does) from the landing advances to the NEXT match, then applies the offset again — matching nvim.
    let b = "abc foo def foo xyz";
    // /foo/e (-> 6) then n (-> 14).
    let e = || soff(SearchOp::Move, 1, "foo", false, SearchOffset::End(0));
    assert_eq!(run(b, &[e()]).cursor().0, 6);
    assert_eq!(run(b, &[e(), e()]).cursor().0, 14);
    // /foo/s+1 (-> 5) then n (-> 13).
    let s1 = || soff(SearchOp::Move, 1, "foo", false, SearchOffset::Start(1));
    assert_eq!(run(b, &[s1()]).cursor().0, 5);
    assert_eq!(run(b, &[s1(), s1()]).cursor().0, 13);
    // /foo/s-1 (-> 3, BEFORE the match) then n still advances to the next match's start-1 (-> 11).
    // This is the case a naive cursor+1 repeat would get stuck on; the offset-position rule fixes it.
    let sm1 = || soff(SearchOp::Move, 1, "foo", false, SearchOffset::Start(-1));
    assert_eq!(run(b, &[sm1()]).cursor().0, 3);
    assert_eq!(run(b, &[sm1(), sm1()]).cursor().0, 11);
}

#[test]
fn search_offset_count_selects_nth_match() {
    // "fooXfooYfooZfoo" — foo starts at 0,4,8,12. 2/foo/e -> 2nd qualifying match's end (nvim [1,6]).
    let b = "fooXfooZfooWfoo";
    assert_eq!(
        run(
            b,
            &[soff(SearchOp::Move, 2, "foo", false, SearchOffset::End(0))]
        )
        .cursor()
        .0,
        6
    );
    // 3/foo/s -> only matches whose start > cursor(0) qualify, so #2/#3/#4 -> the 3rd is at byte 12.
    assert_eq!(
        run(
            b,
            &[soff(
                SearchOp::Move,
                3,
                "foo",
                false,
                SearchOffset::Start(0)
            )]
        )
        .cursor()
        .0,
        12
    );
}

#[test]
fn backward_search_offset() {
    // From end-of-buffer, ?foobar?e -> last char ('r', byte 44) of the match on line 3.
    let mut st = EditorState::new(A.as_bytes().to_vec());
    st.set_cursor(BytePos(A.len() - 1));
    apply_command(
        &mut st,
        &soff(SearchOp::Move, 1, "foobar", true, SearchOffset::End(0)),
    );
    assert_eq!(st.cursor().0, 44, "?foobar?e -> last char of match");

    // d?foobar?s from end -> exclusive [match_start, cursor) deletes "foobar lin" (nvim), leaving
    // "third e" on line 3; the register is charwise.
    let mut st = EditorState::new(A.as_bytes().to_vec());
    st.set_cursor(BytePos(A.len() - 1));
    apply_command(
        &mut st,
        &soff(SearchOp::Delete, 1, "foobar", true, SearchOffset::Start(0)),
    );
    assert_eq!(
        st.as_str().unwrap(),
        "hello world foo\nsecond line here\nthird e"
    );
    assert_eq!(st.register().text(), b"foobar lin");
    assert!(!st.register().is_linewise());

    // d?foobar?e from end -> INCLUSIVE through the cursor char: deletes "r line" (byte 44..=49).
    let mut st = EditorState::new(A.as_bytes().to_vec());
    st.set_cursor(BytePos(A.len() - 1));
    apply_command(
        &mut st,
        &soff(SearchOp::Delete, 1, "foobar", true, SearchOffset::End(0)),
    );
    assert_eq!(
        st.as_str().unwrap(),
        "hello world foo\nsecond line here\nthird fooba"
    );
    assert_eq!(st.register().text(), b"r line");
}

fn gn(op: SearchOp, count: u32, pattern: &str, backward: bool) -> Command {
    Command::SearchObject {
        op,
        count,
        pattern: pattern.to_string(),
        backward,
    }
}

#[test]
fn gn_selects_the_next_match_in_visual() {
    // Cursor at 0 (before any match); `gn` selects the first "foo" as a charwise Visual span.
    let st = run("x foo y foo", &[gn(SearchOp::Move, 1, "foo", false)]);
    assert_eq!(
        st.mode(),
        Mode::Visual {
            kind: SelectKind::Charwise
        }
    );
    assert_eq!(
        st.selection_span(),
        Some((BytePos(2), BytePos(5))),
        "selects the first foo [2,5)"
    );
}

#[test]
fn gn_selects_the_match_under_the_cursor() {
    // Cursor INSIDE the first match (on the 'o' at byte 3) selects THAT match, not the next.
    let st = run(
        "x foo y foo",
        &[
            Command::Move(3, Motion::Right),
            gn(SearchOp::Move, 1, "foo", false),
        ],
    );
    assert_eq!(
        st.selection_span(),
        Some((BytePos(2), BytePos(5))),
        "the containing match, not the next"
    );
}

#[test]
fn dgn_deletes_the_next_whole_match() {
    let st = run("x foo y", &[gn(SearchOp::Delete, 1, "foo", false)]);
    assert_eq!(st.as_str().unwrap(), "x  y");
    assert_eq!(st.mode(), Mode::Normal);
}

#[test]
fn cgn_deletes_the_match_and_enters_insert() {
    let st = run("x foo y", &[gn(SearchOp::Change, 1, "foo", false)]);
    assert_eq!(st.as_str().unwrap(), "x  y");
    assert_eq!(st.mode(), Mode::Insert);
    assert_eq!(
        st.cursor().0,
        2,
        "cursor at the match start, ready to type the replacement"
    );
}

#[test]
fn count_gn_reaches_a_later_match() {
    // `2gn` from the top selects the SECOND match.
    let st = run("a b a b a", &[gn(SearchOp::Move, 2, "a", false)]);
    assert_eq!(
        st.selection_span(),
        Some((BytePos(4), BytePos(5))),
        "the 2nd 'a'"
    );
}

#[test]
fn gn_backward_selects_the_previous_match() {
    // Cursor past the last match; `gN` walks backward to it.
    let st = run(
        "foo bar foo",
        &[
            Command::Move(100, Motion::Right),
            gn(SearchOp::Move, 1, "foo", true),
        ],
    );
    assert_eq!(
        st.selection_span(),
        Some((BytePos(8), BytePos(11))),
        "the last foo, selected backward"
    );
}

#[test]
fn gn_with_no_match_is_a_noop() {
    let st = run("hello", &[gn(SearchOp::Move, 1, "zzz", false)]);
    assert_eq!(st.mode(), Mode::Normal, "no match — stays in Normal");
    assert_eq!(st.cursor().0, 0);
}

/// A search with no previous pattern (an empty one, #1173) fails: a beep, nothing moves, and the wanted
/// column resets to the cursor's (nvim's `normal_search`).
#[test]
fn a_search_with_no_pattern_fails_and_resets_the_wanted_column() {
    let mut st = EditorState::new(b"ab cd ef ghijklmnop qrstuvwxyz\ngh (ij kl\n".to_vec());
    apply_command(&mut st, &Command::Move(1, Motion::Down));
    apply_command(&mut st, &Command::Move(1, Motion::LineEnd));
    let effects = apply_command(
        &mut st,
        &Command::Search {
            op: crate::SearchOp::Delete,
            count: 1,
            pattern: String::new(),
            backward: false,
            offset: crate::SearchOffset::None,
        },
    );
    assert!(effects.contains(&Effect::Beep));
    assert_eq!(st.cursor().0, 31 + 8);
    apply_command(&mut st, &Command::Move(1, Motion::Up));
    assert_eq!(
        st.cursor().0,
        8,
        "the wanted column is the cursor's, not the line's end"
    );
}

fn visual(kind: SelectKind) -> Command {
    Command::EnterVisual { kind }
}

fn charwise() -> Mode {
    Mode::Visual {
        kind: SelectKind::Charwise,
    }
}

/// #723: `gn` in Visual EXTENDS the selection to the match — the anchor stays. nvim 0.12.6:
/// `a foo b` + `/foo<CR>0vgnd` → ` b`, register `a foo`; `0vgn<Esc>` leaves the cursor on col 4.
#[test]
fn visual_gn_extends_the_selection_to_the_match() {
    let mut st = run(
        "a foo b",
        &[
            visual(SelectKind::Charwise),
            gn(SearchOp::Move, 1, "foo", false),
        ],
    );
    assert_eq!(st.mode(), charwise());
    assert_eq!(st.cursor(), BytePos(4));
    assert_eq!(st.selection_span(), Some((BytePos(0), BytePos(5))));
    apply_command(&mut st, &Command::DeleteSelection);
    assert_eq!(st.as_str().unwrap(), " b");
    assert_eq!(st.register().text(), b"a foo");
}

/// #723: `gN` in Visual extends backward to the match start. nvim: `a foo b` + `$vgNd` → `a `, register
/// `foo b`; `$vgN<Esc>` leaves the cursor on col 2.
#[test]
fn visual_gn_backward_extends_to_the_match_start() {
    let mut st = run(
        "a foo b",
        &[
            eol(),
            visual(SelectKind::Charwise),
            gn(SearchOp::Move, 1, "foo", true),
        ],
    );
    assert_eq!(st.cursor(), BytePos(2));
    assert_eq!(st.selection_span(), Some((BytePos(2), BytePos(7))));
    apply_command(&mut st, &Command::DeleteSelection);
    assert_eq!(st.as_str().unwrap(), "a ");
    assert_eq!(st.register().text(), b"foo b");
}

/// #723: a selection that already ends on a match's last char extends to the NEXT match; one ending inside
/// a match finishes that match. nvim: `a foo b foo c` + `0wvllgn<Esc>` → cursor col 10; `0wlvgnd` →
/// `a f b foo c`, register `oo`.
#[test]
fn visual_gn_on_a_covered_match_reaches_the_next() {
    let right = |n| Command::Move(n, Motion::Right);
    let st = run(
        "a foo b foo c",
        &[
            right(2),
            visual(SelectKind::Charwise),
            right(2),
            gn(SearchOp::Move, 1, "foo", false),
        ],
    );
    assert_eq!(st.cursor(), BytePos(10));
    assert_eq!(st.selection_span(), Some((BytePos(2), BytePos(11))));
    let st = run(
        "a foo b foo c",
        &[
            right(3),
            visual(SelectKind::Charwise),
            gn(SearchOp::Move, 1, "foo", false),
        ],
    );
    assert_eq!(st.selection_span(), Some((BytePos(3), BytePos(5))));
}

/// #723: from `V` (or CTRL-V) `gn` switches to charwise Visual and keeps the anchor. nvim:
/// `x\na foo b\ny` + `/foo<CR>ggVgnd` → ` b\ny`, register `x\na foo` charwise.
#[test]
fn visual_line_gn_switches_to_charwise_and_extends() {
    for kind in [SelectKind::Linewise, SelectKind::Blockwise] {
        let mut st = run(
            "x\na foo b\ny",
            &[visual(kind), gn(SearchOp::Move, 1, "foo", false)],
        );
        assert_eq!(st.mode(), charwise(), "{kind:?}");
        assert_eq!(st.cursor(), BytePos(6), "{kind:?}");
        apply_command(&mut st, &Command::DeleteSelection);
        assert_eq!(st.as_str().unwrap(), " b\ny", "{kind:?}");
        assert_eq!(st.register().text(), b"x\na foo", "{kind:?}");
        assert!(!st.register().is_linewise(), "{kind:?}");
    }
}

/// #723: a count advances matches, and the search wraps; with the cursor past the last match `gn` wraps to
/// the first one and lands on its START (the match is before the anchor). nvim: `a foo b foo c foo d` +
/// `0v2gnd` → ` c foo d`; `a foo b foo c` + `$vgn<Esc>` → cursor col 2, selection 2..12; `0vgNd` →
/// `oo c`.
#[test]
fn visual_gn_counts_and_wraps() {
    let mut st = run(
        "a foo b foo c foo d",
        &[
            visual(SelectKind::Charwise),
            gn(SearchOp::Move, 2, "foo", false),
        ],
    );
    apply_command(&mut st, &Command::DeleteSelection);
    assert_eq!(st.as_str().unwrap(), " c foo d");
    let st = run(
        "a foo b foo c",
        &[
            eol(),
            visual(SelectKind::Charwise),
            gn(SearchOp::Move, 1, "foo", false),
        ],
    );
    assert_eq!(st.cursor(), BytePos(2));
    assert_eq!(st.selection_span(), Some((BytePos(2), BytePos(13))));
    let mut st = run(
        "a foo b foo c",
        &[
            visual(SelectKind::Charwise),
            gn(SearchOp::Move, 1, "foo", true),
        ],
    );
    assert_eq!(st.cursor(), BytePos(8));
    apply_command(&mut st, &Command::DeleteSelection);
    assert_eq!(st.as_str().unwrap(), "oo c");
}

/// #723: with the cursor before the anchor, `gn` takes the first match starting after the char past the
/// cursor and puts the cursor on its start. nvim: `afoo bfoo c` + `$v0gnd` → `afoo b`, register `foo c`.
#[test]
fn visual_gn_with_the_cursor_before_the_anchor() {
    let mut st = run(
        "afoo bfoo c",
        &[
            eol(),
            visual(SelectKind::Charwise),
            Command::Move(1, Motion::LineStart),
            gn(SearchOp::Move, 1, "foo", false),
        ],
    );
    assert_eq!(st.cursor(), BytePos(6));
    apply_command(&mut st, &Command::DeleteSelection);
    assert_eq!(st.as_str().unwrap(), "afoo b");
    assert_eq!(st.register().text(), b"foo c");
}

/// #723: no match fails and keeps the selection as it was. nvim: `a bar b` + `0vlgn<Esc>` → cursor col 1,
/// `'<`/`'>` 0..1.
#[test]
fn visual_gn_with_no_match_keeps_the_selection() {
    let st = run(
        "a bar b",
        &[
            visual(SelectKind::Charwise),
            Command::Move(1, Motion::Right),
            gn(SearchOp::Move, 1, "foo", false),
        ],
    );
    assert_eq!(st.mode(), charwise());
    assert_eq!(st.selection_span(), Some((BytePos(0), BytePos(2))));
}

/// Bare `gN` from Normal anchors on the match's last char with the cursor on its start (nvim: `a foo b` +
/// `$gN<Esc>` → cursor col 2).
#[test]
fn gn_backward_from_normal_puts_the_cursor_on_the_match_start() {
    let st = run("a foo b", &[eol(), gn(SearchOp::Move, 1, "foo", true)]);
    assert_eq!(st.cursor(), BytePos(2));
    assert_eq!(st.selection_span(), Some((BytePos(2), BytePos(5))));
}
