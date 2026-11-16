//! Text queries at the cursor and completion.

use super::*;
use crate::editor::SubRange;

/// `*`/`#` word extraction: the keyword under the cursor, or the next one forward on the line; and a
/// whole-word `\<…\>` search (as `*` builds) skips substring hits.
#[test]
fn word_under_cursor_and_whole_word_search() {
    // On a keyword → that word.
    let w = Workspace::new(b"foo bar\n".to_vec());
    assert_eq!(w.word_under_cursor().as_deref(), Some("foo"));

    // On whitespace → the next keyword forward on the line.
    let mut w = Workspace::new(b"foo bar\n".to_vec());
    w.place_focused_cursor(BytePos(3)); // the space
    assert_eq!(w.word_under_cursor().as_deref(), Some("bar"));

    // No keyword on the line → None.
    let w = Workspace::new(b"!!! ???\n".to_vec());
    assert_eq!(w.word_under_cursor(), None);
    assert_eq!(Workspace::new(b"".to_vec()).word_under_cursor(), None);

    // The whole-word pattern `*` builds skips the substring hit ("foobar") and lands on the next
    // standalone "foo".
    let mut w = Workspace::new(b"foo foobar foo\n".to_vec());
    w.apply(&Command::SearchNext("\\<foo\\>".to_string()));
    assert_eq!(
        w.focused().view.cursor().0,
        11,
        "whole-word search skips foobar"
    );
}

/// `<cword>` / `<cWORD>` under the caret — the text `c_CTRL-R_CTRL-W` / `c_CTRL-R_CTRL-A` splice into
/// the command line. Every case below was captured from nvim v0.12.4 (`expand('<cword>')` /
/// `expand('<cWORD>')` with the caret placed identically).
#[test]
fn cword_and_cbig_word_under_cursor() {
    // (line, byte-cursor, expected <cword>, expected <cWORD>)
    type Case<'a> = (&'a [u8], usize, Option<&'a str>, Option<&'a str>);
    let cases: &[Case] = &[
        // On / mid a plain keyword → that keyword for both.
        (b"foobar baz\n", 0, Some("foobar"), Some("foobar")),
        (b"foobar baz\n", 3, Some("foobar"), Some("foobar")),
        // `foo.bar`: cword is the keyword touching the caret; cWORD is the whole non-blank run.
        (b"foo.bar baz\n", 0, Some("foo"), Some("foo.bar")), // on 'f'
        (b"foo.bar baz\n", 3, Some("bar"), Some("foo.bar")), // on '.', keyword forward = "bar"
        // On leading whitespace → the next run forward on the line.
        (b"  hello world\n", 0, Some("hello"), Some("hello")),
        // cword crosses blanks+punct to the next keyword; cWORD stops at the run under the caret.
        (b". foo\n", 0, Some("foo"), Some(".")),
        // No keyword on the rest of the line: cword falls back to the non-blank (punctuation) run.
        (b"foo ...\n", 4, Some("..."), Some("...")),
        (b"end.\n", 3, Some("."), Some("end.")), // on the trailing '.'
        // Rest of the line blank → nothing under the cursor for either.
        (b"x  \n", 1, None, None),
        (b"  \n", 0, None, None),
        // Non-ASCII is keyword-class; `_`/digits too.
        ("café x\n".as_bytes(), 2, Some("café"), Some("café")), // caret on the 'f'
        (b"a1_b2 z\n", 0, Some("a1_b2"), Some("a1_b2")),
        // Empty buffer.
        (b"", 0, None, None),
    ];
    for (line, cur, cword, cbig) in cases {
        let mut w = Workspace::new(line.to_vec());
        w.place_focused_cursor(BytePos(*cur));
        assert_eq!(
            w.cword_under_cursor().as_deref(),
            *cword,
            "<cword> for {:?} at {cur}",
            String::from_utf8_lossy(line)
        );
        assert_eq!(
            w.cbig_word_under_cursor().as_deref(),
            *cbig,
            "<cWORD> for {:?} at {cur}",
            String::from_utf8_lossy(line)
        );
    }
}

/// `keyword_completion` backs `i_CTRL-N`/`i_CTRL-P`: the base (keyword before the caret) plus the
/// buffer keywords that start with it, in nvim scan order (forward-from-caret then wrap, the word
/// under construction excluded, duplicates removed). Cases captured from nvim v0.12.4.
#[test]
fn keyword_completion_base_and_candidates() {
    // Base "fo" on a fresh last line: forward scan finds nothing, wraps from the top; the two "foo"
    // occurrences dedupe. (nvim: match set = foo, food, foobar.)
    let mut w = Workspace::new(b"foo food\nfoobar\nfoo\nfo".to_vec());
    w.place_focused_cursor(BytePos(w.focused().doc.bytes().len())); // end of the "fo" line
    let (base, cands) = w.keyword_completion();
    assert_eq!(base, "fo");
    assert_eq!(cands, vec!["foo", "food", "foobar"]);

    // Mid-buffer caret: forward to end, then wrap from the top; the caret's own "aa" is excluded.
    let mut w = Workspace::new(b"aa1 aa2\naa3\naa\naa4 aa5\naa6".to_vec());
    w.place_focused_cursor(BytePos(14)); // end of the bare "aa" on line 3
    let (base, cands) = w.keyword_completion();
    assert_eq!(base, "aa");
    assert_eq!(cands, vec!["aa4", "aa5", "aa6", "aa1", "aa2", "aa3"]);

    // Exact match is a candidate distinct from the original text.
    let mut w = Workspace::new(b"foo foobar\nfoo".to_vec());
    w.place_focused_cursor(BytePos(w.focused().doc.bytes().len()));
    let (base, cands) = w.keyword_completion();
    assert_eq!(base, "foo");
    assert_eq!(cands, vec!["foo", "foobar"]);

    // No match → empty candidate list (a Vim no-op / bell).
    let mut w = Workspace::new(b"foo bar\nzz".to_vec());
    w.place_focused_cursor(BytePos(w.focused().doc.bytes().len()));
    let (base, cands) = w.keyword_completion();
    assert_eq!(base, "zz");
    assert!(cands.is_empty());

    // Empty base (caret after a non-word char) matches every keyword, none excluded.
    let mut w = Workspace::new(b"foo bar\n(".to_vec());
    w.place_focused_cursor(BytePos(w.focused().doc.bytes().len())); // just after '('
    let (base, cands) = w.keyword_completion();
    assert_eq!(base, "");
    assert_eq!(cands, vec!["foo", "bar"]);
}

/// `line_completion` backs `i_CTRL-X CTRL-L`: the post-indent prefix before the caret plus the other
/// buffer lines' post-indent content that start with it, in Vim's backward-from-caret-with-wrap order,
/// #1252: a line or keyword holding a byte that is not UTF-8 is a candidate, spelled (nvim 0.12.6
/// offers `x<ff>y` to `x` `CTRL-X CTRL-L`); `CTRL-Y`/`CTRL-E` read the char at the caret's virtual
/// column and give such a byte as its Latin-1 char, which nvim inserts as UTF-8.
#[test]
fn completion_and_copy_char_keep_an_invalid_byte() {
    let spelled = |b: &[u8]| crate::pattern::spell(b).into_owned();
    let mut w = Workspace::new(b"x\xffy\nxyz\nx".to_vec());
    w.place_focused_cursor(BytePos(9));
    assert_eq!(
        w.line_completion(),
        ("x".to_string(), vec!["xyz".to_string(), spelled(b"x\xffy")])
    );
    let mut w = Workspace::new(b"\xffabcdef\nx\n\tabc\nx\n\xe4\xb8\xadab\nx\n".to_vec());
    // `x` then the caret at virtual column 1: inside `<ff>` (cells 0-3), the tab, the wide char.
    for (at, want) in [(9, '\u{ff}'), (16, '\t'), (24, '中')] {
        w.place_focused_cursor(BytePos(at));
        assert_eq!(w.adjacent_line_char(true), Some(want), "caret at {at}");
    }
    let mut w = Workspace::new(b"\nab\xffcd\n".to_vec());
    w.place_focused_cursor(BytePos(0));
    assert_eq!(w.adjacent_line_char(false), Some('a'));
}

/// deduped by content (indent ignored). Cases captured from nvim v0.12.4.
#[test]
fn line_completion_base_and_candidates() {
    // Prefix "foobar" on a fresh last line: backward scan (the two identical "foobar baz" dedupe).
    let mut w = Workspace::new(b"foobar baz\nfoobar qux\nfoobar baz\nhello world\nfoobar".to_vec());
    w.place_focused_cursor(BytePos(w.focused().doc.bytes().len()));
    let (base, cands) = w.line_completion();
    assert_eq!(base, "foobar");
    assert_eq!(cands, vec!["foobar baz", "foobar qux"]);

    // Mid-buffer caret: backward to the top, then wrap from the bottom upward; the caret line excluded.
    let mut w = Workspace::new(b"foo A\nfoo B\nfoo\nfoo C\nfoo D".to_vec());
    w.place_focused_cursor(BytePos(15)); // end of the bare "foo" on line 3 (0-based row 2)
    let (base, cands) = w.line_completion();
    assert_eq!(base, "foo");
    assert_eq!(cands, vec!["foo B", "foo A", "foo D", "foo C"]);

    // Indent is ignored for matching and dedup: "foo bar" and "    foo bar" collapse to one candidate.
    let mut w = Workspace::new(b"foo bar\n    foo bar\nfoo baz\nfoo".to_vec());
    w.place_focused_cursor(BytePos(w.focused().doc.bytes().len()));
    let (base, cands) = w.line_completion();
    assert_eq!(base, "foo");
    assert_eq!(cands, vec!["foo baz", "foo bar"]);

    // The prefix's own indent is stripped from `base` (its length is what the completion deletes).
    let mut w = Workspace::new(b"    indented alpha\nplain beta\n  in".to_vec());
    w.place_focused_cursor(BytePos(w.focused().doc.bytes().len()));
    let (base, cands) = w.line_completion();
    assert_eq!(base, "in");
    assert_eq!(cands, vec!["indented alpha"]);

    // No matching line → empty candidate list (a Vim no-op / bell).
    let mut w = Workspace::new(b"foo bar\nbaz\nzz".to_vec());
    w.place_focused_cursor(BytePos(w.focused().doc.bytes().len()));
    let (base, cands) = w.line_completion();
    assert_eq!(base, "zz");
    assert!(cands.is_empty());
}

/// `adjacent_line_char` backs `i_CTRL-E`/`i_CTRL-Y`: the char below/above the caret at its column, or
/// `None` for a short/absent adjacent line (a Vim no-op). Column = char count before the caret.
#[test]
fn adjacent_line_char_below_and_above() {
    let mut w = Workspace::new(b"hello\nworld\n".to_vec());
    // Caret at col 0 of line 1: below = 'w', above = None (no line above).
    assert_eq!(w.adjacent_line_char(false), Some('w'));
    assert_eq!(w.adjacent_line_char(true), None);
    // Caret at col 2 of line 1 ('l'): below 'world' at col 2 = 'r'.
    w.place_focused_cursor(BytePos(2));
    assert_eq!(w.adjacent_line_char(false), Some('r'));
    // Caret on line 2, col 0: above 'hello' col 0 = 'h', below = None (no line below).
    w.place_focused_cursor(BytePos(6));
    assert_eq!(w.adjacent_line_char(true), Some('h'));
    assert_eq!(w.adjacent_line_char(false), None);
    // Column past the end of the adjacent (shorter) line → None.
    let mut w = Workspace::new(b"hello\nab\n".to_vec());
    w.place_focused_cursor(BytePos(4)); // col 4 of "hello"; "ab" has no col 4
    assert_eq!(w.adjacent_line_char(false), None);
}

/// `gd`/`gD` (go-to-declaration, TEXT heuristic): the frontend reads the keyword under the cursor and
/// rewrites to `GotoFirstMatch("\<word\>")`, which lands on the FIRST whole-word match from the TOP of
/// the file (matching nvim v0.12.4 for both `gd` and `gD`, verified against nvim). It is a JUMP, so the
/// leaving position is recorded and `CTRL-O` returns. No match keeps the cursor.
#[test]
fn goto_declaration_first_match_from_top_and_jumplist() {
    // Buffer: `int foo;` then two uses of `foo`. Cursor on the LAST `foo` (line 3, "int baz = foo;").
    let src = b"int foo;\nvoid bar() { foo = 1; }\nint baz = foo;\n".to_vec();
    let decl = src
        .windows(3)
        .position(|w| w == b"foo")
        .expect("foo present"); // byte offset of the first `foo` (in `int foo;`)
    let last_foo = src
        .windows(3)
        .enumerate()
        .rfind(|(_, w)| *w == b"foo")
        .expect("foo present")
        .0;

    let mut w = Workspace::new(src.clone());
    w.place_focused_cursor(BytePos(last_foo));
    // Mirror the frontend rewrite (session.rs): keyword under cursor → whole-word first-match jump.
    let word = w.word_under_cursor().expect("on a keyword");
    assert_eq!(word, "foo");
    w.apply(&Command::GotoFirstMatch(format!("\\<{word}\\>")));
    assert_eq!(
        w.focused().view.cursor().0,
        decl,
        "gd/gD lands on the first whole-word match from the top of the file"
    );
    // It was a JUMP: the leaving position (last `foo`) is on the jumplist and `CTRL-O` returns there.
    assert!(
        w.jumps_snapshot().contains(&BytePos(last_foo)),
        "the jump records the position it left"
    );
    w.apply(&Command::GotoOlderJump); // CTRL-O
    assert_eq!(
        w.focused().view.cursor().0,
        last_foo,
        "CTRL-O returns to where gd/gD jumped from"
    );

    // No match → the cursor stays put (Vim rings the bell).
    let mut w = Workspace::new(src);
    w.place_focused_cursor(BytePos(last_foo));
    w.apply(&Command::GotoFirstMatch("\\<nope\\>".to_string()));
    assert_eq!(
        w.focused().view.cursor().0,
        last_foo,
        "no match leaves the cursor put"
    );

    // The whole-word pattern skips substrings: `foobar` is not a match for `\<foo\>`.
    let mut w = Workspace::new(b"foobar\nfoo\n".to_vec());
    w.place_focused_cursor(BytePos(7)); // on the standalone `foo` (line 2)
    w.apply(&Command::GotoFirstMatch("\\<foo\\>".to_string()));
    assert_eq!(
        w.focused().view.cursor().0,
        7,
        "the first WHOLE-word match is the standalone foo, not the foobar substring"
    );
}

/// A pathological `{count}p` (a digit-spam count that saturates to `u32::MAX`) must not request a
/// multi-gigabyte allocation: the paste is clamped to a bounded number of bytes and completes.
#[test]
fn huge_paste_count_is_bounded() {
    let mut w = Workspace::new(b"ab\n".to_vec());
    w.yank_lines(SubRange::CurrentLine); // register := "ab\n" (linewise)
    w.apply(&Command::Paste {
        after: true,
        count: u32::MAX,
        move_after: false,
    });
    // Bounded to ~64 MiB, NOT 3 × 4.29e9 ≈ 12 GiB.
    assert!(
        w.focused().doc.bytes().len() <= (1 << 26) + 16,
        "paste bytes are clamped, not unbounded"
    );
}
