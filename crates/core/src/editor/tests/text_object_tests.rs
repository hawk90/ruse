//! Text objects, tag objects included. Split from `editor/tests.rs` (#1397).

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
fn counted_word_objects() {
    // Verified vs nvim v0.12.4. `aw` count = N words+trailing-ws; `iw` count = N alternating class runs.
    let st = run("foo bar baz qux", &[Command::Delete(2, Motion::AWord)]);
    assert_eq!(
        text(&st),
        "baz qux",
        "d2aw deletes two words with trailing ws"
    );
    let st = run("foo bar baz", &[Command::Delete(2, Motion::InnerWord)]);
    assert_eq!(
        text(&st),
        "bar baz",
        "d2iw = word + following whitespace run"
    );
    let st = run("foo bar baz", &[Command::Delete(3, Motion::InnerWord)]);
    assert_eq!(text(&st), " baz", "d3iw = word + ws + word");
    let st = run("foo.bar baz qux", &[Command::Delete(2, Motion::ABigWord)]);
    assert_eq!(
        text(&st),
        "qux",
        "d2aW spans two WHITESPACE-delimited words"
    );
    // Change variant collapses correctly.
    let st = run("foo bar baz", &[Command::Change(2, Motion::InnerWord)]);
    assert_eq!(text(&st), "bar baz", "c2iw removes 'foo ' then inserts");
    assert_eq!(st.mode(), Mode::Insert);
}

fn pair(open: char, close: char, around: bool) -> Motion {
    Motion::Pair {
        open,
        close,
        around,
    }
}

#[test]
fn iw_splits_on_punctuation_but_big_word_does_not() {
    // cursor on 'f' of "foo.bar": `iw` is the word class run "foo"; `iW` is the whole WORD "foo.bar".
    let st = run("foo.bar", &[Command::Delete(1, Motion::InnerWord)]);
    assert_eq!(text(&st), ".bar", "diw stops at the punctuation");
    let st = run("foo.bar baz", &[Command::Delete(1, Motion::InnerBigWord)]);
    assert_eq!(text(&st), " baz", "diW spans the punctuation");
}

#[test]
fn aw_and_a_big_word_take_trailing_whitespace() {
    let st = run("foo bar baz", &[Command::Delete(1, Motion::AWord)]);
    assert_eq!(
        text(&st),
        "bar baz",
        "daw removes the word and its trailing space"
    );
    let st = run("foo.bar baz", &[Command::Delete(1, Motion::ABigWord)]);
    assert_eq!(
        text(&st),
        "baz",
        "daW removes the WORD and its trailing space"
    );
}

#[test]
fn change_paragraph_is_linewise() {
    // `cip`/`cap` are LINEWISE changes (paragraphs are linewise): collapse the paragraph's lines to one
    // empty line and enter Insert, register linewise. Verified vs nvim v0.12.4 (fixture cip_*).
    let st = run(
        "one\ntwo\n\nthree",
        &[Command::Change(1, Motion::InnerParagraph)],
    );
    assert_eq!(
        text(&st),
        "\n\nthree",
        "cip clears the paragraph to one empty line"
    );
    assert_eq!(st.mode(), Mode::Insert);
    assert!(st.register().is_linewise(), "cip register is linewise");
    assert_eq!(st.register().text(), b"one\ntwo\n");
}

#[test]
fn inner_block_on_own_lines_is_linewise() {
    // Vim: `di(`/`ci(` on a block whose braces are on their own lines act LINEWISE. Expects captured
    // from nvim v0.12.4 (oracle fixtures di_paren_multiline / ci_paren_multiline / ci_brace_*).
    // `di(` removes the whole inner line(s).
    let st = run(
        "foo(\nbar\n)baz",
        &[Command::Delete(1, pair('(', ')', false))],
    );
    assert_eq!(text(&st), "foo(\n)baz", "di( deletes the whole inner line");
    assert!(st.register().is_linewise(), "register is linewise");
    assert_eq!(st.register().text(), b"bar\n");
    // `ci(` collapses the inner to ONE empty line (like cc) and enters Insert.
    let st = run(
        "foo(\nbar\n)baz",
        &[Command::Change(1, pair('(', ')', false))],
    );
    assert_eq!(text(&st), "foo(\n\n)baz", "ci( leaves one empty inner line");
    assert_eq!(st.mode(), Mode::Insert);
    // Multiple inner lines collapse to one; first line's indent is PRESERVED (cc semantics).
    let st = run(
        "fn(){\n    body\n}",
        &[Command::MoveDown, Command::Change(1, pair('{', '}', false))],
    );
    assert_eq!(
        text(&st),
        "fn(){\n    \n}",
        "ci{{ keeps the indent, clears the content"
    );
    // `da(` (around) stays CHARWISE even multiline — deletes through both braces on one line.
    let st = run(
        "foo(\nbar\n)baz",
        &[Command::Delete(1, pair('(', ')', true))],
    );
    assert_eq!(
        text(&st),
        "foobaz",
        "da( is charwise across the whole block"
    );
    assert!(!st.register().is_linewise());
    // Content sharing the open/close line stays CHARWISE (not the special case).
    let st = run(
        "foo(bar\nbaz)qux",
        &[Command::Delete(1, pair('(', ')', false))],
    );
    assert_eq!(text(&st), "foo()qux", "inline-open block stays charwise");
}

#[test]
fn delimiter_pair_inner_and_around() {
    // cursor inside the parens of "a(bc)d".
    let st = run(
        "a(bc)d",
        &[
            Command::Move(2, Motion::Right),
            Command::Delete(1, pair('(', ')', false)),
        ],
    );
    assert_eq!(text(&st), "a()d", "di( deletes the interior");
    let st = run(
        "a(bc)d",
        &[
            Command::Move(2, Motion::Right),
            Command::Delete(1, pair('(', ')', true)),
        ],
    );
    assert_eq!(text(&st), "ad", "da( deletes the delimiters too");
}

#[test]
fn delimiter_pair_is_nesting_aware() {
    // On the inner content of "(a(b)c)": from the outer, di( takes everything inside the OUTER pair.
    let st = run("(a(b)c)", &[Command::Delete(1, pair('(', ')', false))]);
    assert_eq!(
        text(&st),
        "()",
        "di( on the opener spans to the matching closer"
    );
    // Cursor inside the inner pair selects only the inner interior.
    let st = run(
        "(a(b)c)",
        &[
            Command::Move(3, Motion::Right),
            Command::Delete(1, pair('(', ')', false)),
        ],
    );
    assert_eq!(
        text(&st),
        "(a()c)",
        "di( from inside the inner pair takes only 'b'"
    );
}

#[test]
fn delimiter_object_outside_a_pair_is_a_noop() {
    let st = run("abc", &[Command::Delete(1, pair('(', ')', false))]);
    assert_eq!(text(&st), "abc");
}

#[test]
fn quote_inner_and_around() {
    // `a "hi" b`: quotes at 2 and 5; cursor on 'h'.
    let st = run(
        "a \"hi\" b",
        &[
            Command::Move(3, Motion::Right),
            Command::Change(
                1,
                Motion::Quote {
                    ch: '"',
                    around: false,
                },
            ),
        ],
    );
    assert_eq!(text(&st), "a \"\" b", "ci\" clears the interior");
    assert_eq!(st.mode(), Mode::Insert);
    let st = run(
        "a \"hi\" b",
        &[
            Command::Move(3, Motion::Right),
            Command::Delete(
                1,
                Motion::Quote {
                    ch: '"',
                    around: true,
                },
            ),
        ],
    );
    assert_eq!(
        text(&st),
        "a b",
        "da\" removes the quotes and the trailing space"
    );
}

#[test]
fn quotes_are_single_line() {
    // The quote on the next line must not pair with one on this line.
    let st = run(
        "x\"a\nb\"y",
        &[Command::Delete(
            1,
            Motion::Quote {
                ch: '"',
                around: false,
            },
        )],
    );
    assert_eq!(
        text(&st),
        "x\"a\nb\"y",
        "no matching quote on this line → no-op"
    );
}

#[test]
fn paragraph_inner_and_around() {
    // "l1\nl2\n\nl3\n": cursor in the first paragraph.
    let st = run(
        "l1\nl2\n\nl3\n",
        &[Command::Delete(1, Motion::InnerParagraph)],
    );
    assert_eq!(text(&st), "\nl3\n", "dip removes the paragraph's lines");
    let st = run("l1\nl2\n\nl3\n", &[Command::Delete(1, Motion::AParagraph)]);
    assert_eq!(
        text(&st),
        "l3\n",
        "dap also removes the trailing blank line"
    );
}

#[test]
fn sentence_inner_and_around() {
    let st = run("One. Two.", &[Command::Delete(1, Motion::InnerSentence)]);
    assert_eq!(
        text(&st),
        " Two.",
        "dis removes the first sentence, keeping the space"
    );
    let st = run("One. Two.", &[Command::Delete(1, Motion::ASentence)]);
    assert_eq!(
        text(&st),
        "Two.",
        "das removes the sentence and its trailing space"
    );
}

#[test]
fn text_object_selects_in_visual() {
    // `viw` spans the word under the cursor.
    let st = run(
        "foo bar",
        &[
            Command::EnterVisual {
                kind: SelectKind::Charwise,
            },
            Command::Move(1, Motion::InnerWord),
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
        "viw selects 'foo'"
    );
    // `vi(` spans the interior of the enclosing pair.
    let st = run(
        "a(bc)d",
        &[
            Command::Move(2, Motion::Right),
            Command::EnterVisual {
                kind: SelectKind::Charwise,
            },
            Command::Move(1, pair('(', ')', false)),
        ],
    );
    assert_eq!(
        st.selection_span(),
        Some((BytePos(2), BytePos(4))),
        "vi( selects 'bc'"
    );
}

#[cfg(test)]
mod tag_text_object_tests {
    //! Vim `it`/`at` — HTML/XML tag text objects (`dit`/`dat`/`cit`/`yat`/`vit`…). Every `assert` is
    //! pinned to nvim v0.12.4 (probed directly; the `dit_*`/`dat_*`/`cit_*`/`vat_*`/`nit_*` oracle
    //! fixtures in tests/parity/vim/fixtures/corpus.yaml carry the machine-captured ground truth).
    use crate::editor::*;

    fn at(initial: &str, cur: usize, cmds: &[Command]) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        st.set_cursor(BytePos(cur));
        for c in cmds {
            apply_command(&mut st, c);
        }
        st
    }
    fn text(st: &EditorState) -> String {
        String::from_utf8(st.bytes().to_vec()).expect("utf8")
    }
    fn dit() -> Command {
        Command::Delete(1, Motion::Tag { around: false })
    }
    fn dat() -> Command {
        Command::Delete(1, Motion::Tag { around: true })
    }

    #[test]
    fn dit_deletes_inner_dat_deletes_whole_block() {
        // <div>hello</div>: cursor on the 'h' (byte 5).
        let st = at("<div>hello</div>", 5, &[dit()]);
        assert_eq!(text(&st), "<div></div>");
        assert_eq!(st.register().text(), b"hello");
        assert!(!st.register().is_linewise());

        let st = at("<div>hello</div>", 5, &[dat()]);
        assert_eq!(text(&st), "");
        assert_eq!(st.register().text(), b"<div>hello</div>");
    }

    #[test]
    fn cursor_on_either_tag_still_resolves_the_block() {
        // On the opening tag (byte 1, inside "<div>").
        assert_eq!(text(&at("<div>hello</div>", 1, &[dit()])), "<div></div>");
        // On the closing tag (byte 12, inside "</div>").
        assert_eq!(text(&at("<div>hello</div>", 12, &[dit()])), "<div></div>");
    }

    #[test]
    fn nesting_targets_the_innermost_enclosing_tag() {
        // <a><b>x</b></a>: cursor on 'x' (byte 6) → innermost is <b>.
        let st = at("<a><b>x</b></a>", 6, &[dit()]);
        assert_eq!(text(&st), "<a><b></b></a>");
        assert_eq!(st.register().text(), b"x");

        let st = at("<a><b>x</b></a>", 6, &[dat()]);
        assert_eq!(text(&st), "<a></a>");
        assert_eq!(st.register().text(), b"<b>x</b>");
    }

    #[test]
    fn count_expands_outward_one_nesting_level_at_a_time() {
        // 2dit = inner of the SECOND-level-out tag (<a>) = "<b>x</b>"; 2dat = the whole <a> block.
        let st = at(
            "<a><b>x</b></a>",
            6,
            &[Command::Delete(2, Motion::Tag { around: false })],
        );
        assert_eq!(text(&st), "<a></a>");
        assert_eq!(st.register().text(), b"<b>x</b>");

        let st = at(
            "<a><b>x</b></a>",
            6,
            &[Command::Delete(2, Motion::Tag { around: true })],
        );
        assert_eq!(text(&st), "");
        assert_eq!(st.register().text(), b"<a><b>x</b></a>");

        // Three levels deep: 3dit climbs to the OUTERMOST tag's inner.
        let st = at(
            "<a><b><c>x</c></b></a>",
            9,
            &[Command::Delete(3, Motion::Tag { around: false })],
        );
        assert_eq!(text(&st), "<a></a>");
        assert_eq!(st.register().text(), b"<b><c>x</c></b>");
    }

    #[test]
    fn count_beyond_nesting_depth_is_a_noop() {
        // 3dit on a 2-deep block: no third enclosing level → nothing happens (matches nvim).
        let st = at(
            "<a><b>x</b></a>",
            6,
            &[Command::Delete(3, Motion::Tag { around: false })],
        );
        assert_eq!(text(&st), "<a><b>x</b></a>");
        assert!(st.register().is_empty());
    }

    #[test]
    fn attributes_and_odd_names_in_the_open_tag_are_matched_by_name() {
        // dat with attributes: the opening tag's attrs are part of the block, close matched by name.
        let st = at(r#"<a href="x">hi</a>"#, 12, &[dat()]);
        assert_eq!(text(&st), "");
        assert_eq!(st.register().text(), br#"<a href="x">hi</a>"#);
        // Hyphenated element name.
        let st = at("<my-el>hi</my-el>", 7, &[dit()]);
        assert_eq!(text(&st), "<my-el></my-el>");
    }

    #[test]
    fn cursor_outside_any_pair_is_a_noop() {
        // In the whitespace between two sibling tags — inside neither.
        let st = at("<a>1</a> <b>2</b>", 8, &[dit()]);
        assert_eq!(text(&st), "<a>1</a> <b>2</b>");
        assert!(st.register().is_empty());
        // Plain text, no tags at all.
        let st = at("plain text", 3, &[dat()]);
        assert_eq!(text(&st), "plain text");
        assert!(st.register().is_empty());
        // A self-closing tag has no content to enclose the cursor.
        let st = at("<br/>x", 5, &[dat()]);
        assert_eq!(text(&st), "<br/>x");
    }

    #[test]
    fn multiline_block_stays_charwise() {
        // <div>\n  hello\n</div>, cursor on line 2 (byte 8). nvim keeps `it`/`at`'s YANK/CHANGE span
        // CHARWISE even when the tags sit on their own lines (unlike `di(`), so ruse — which uses one
        // span for d/y/c — is charwise throughout. The visible edit still collapses the block.
        let src = "<div>\n  hello\n</div>";
        let st = at(src, 8, &[dit()]);
        assert_eq!(text(&st), "<div></div>");
        assert!(!st.register().is_linewise(), "dit multiline stays charwise");
        assert_eq!(st.register().text(), b"\n  hello\n");

        // `dat` on a whole-line block deletes LINEWISE (Vi's `d` quirk, nvim `op_delete`: a multi-line
        // charwise delete starting in the indent with only blanks after it), while `yat`/`cat` stay
        // charwise. Pinned against the oracle by the `dat_tag_multiline` fixture.
        let st = at(src, 8, &[dat()]);
        assert_eq!(text(&st), "");
        assert!(
            st.register().is_linewise(),
            "dat over whole lines is linewise"
        );
    }

    #[test]
    fn change_inner_deletes_content_and_enters_insert() {
        let mut st = EditorState::new(b"<div>hello</div>".to_vec());
        st.set_cursor(BytePos(5));
        apply_command(&mut st, &Command::Change(1, Motion::Tag { around: false }));
        assert_eq!(text(&st), "<div></div>");
        assert_eq!(st.mode(), Mode::Insert);
    }

    #[test]
    fn visual_it_selects_inner_and_at_selects_the_whole_block() {
        // vit then delete = dit; vat then delete = dat (the text object drives BOTH selection ends).
        let st = at(
            "<div>hello</div>",
            5,
            &[
                Command::EnterVisual {
                    kind: SelectKind::Charwise,
                },
                Command::Move(1, Motion::Tag { around: false }),
                Command::DeleteSelection,
            ],
        );
        assert_eq!(text(&st), "<div></div>");

        let st = at(
            "<div>hello</div>",
            5,
            &[
                Command::EnterVisual {
                    kind: SelectKind::Charwise,
                },
                Command::Move(1, Motion::Tag { around: true }),
                Command::DeleteSelection,
            ],
        );
        assert_eq!(text(&st), "");
    }

    #[test]
    fn visual_count_it_expands_to_an_outer_level() {
        // v2it selects the inner content of the second-level-out tag (users reach outer levels via a
        // count; bare repeated `it` re-expansion in Visual is a documented v0 limitation).
        let st = at(
            "<a><b>x</b></a>",
            6,
            &[
                Command::EnterVisual {
                    kind: SelectKind::Charwise,
                },
                Command::Move(2, Motion::Tag { around: false }),
                Command::DeleteSelection,
            ],
        );
        assert_eq!(text(&st), "<a></a>");
        assert_eq!(st.register().text(), b"<b>x</b>");
    }
}

/// #1179: an object with nothing to select under an operator, and `~` on an empty line, fail (a beep),
/// so the frontend keeps the earlier change for `.`; an object that selects nothing (`di(` on `()`) runs.
#[test]
fn failed_objects_and_tilde_beep() {
    let pair = Motion::Pair {
        open: '(',
        close: ')',
        around: false,
    };
    for (text, cmd) in [
        (&b"abc def"[..], Command::Delete(1, pair)),
        (b"abc def", Command::Change(1, pair)),
        (
            b"abc def",
            Command::Delete(
                1,
                Motion::Quote {
                    ch: '"',
                    around: true,
                },
            ),
        ),
        (
            b"abc def",
            Command::Delete(1, Motion::Tag { around: false }),
        ),
        (
            b"<a>x</a> def",
            Command::Delete(2, Motion::Tag { around: true }),
        ),
        (b"\nabc", Command::ToggleCase(1)),
    ] {
        let mut st = EditorState::new(text.to_vec());
        let effects = apply_command(&mut st, &cmd);
        assert_eq!(effects, vec![Effect::Beep], "{cmd:?}");
        assert_eq!(st.doc.bytes(), text, "{cmd:?}");
        assert_eq!(st.view.mode, Mode::Normal, "{cmd:?}");
    }
    let mut st = EditorState::new(b"a() b".to_vec());
    st.view.cursor = 2;
    assert!(!apply_command(&mut st, &Command::Delete(1, pair)).contains(&Effect::Beep));
    let mut st = EditorState::new(b"<a>x</a>".to_vec());
    st.view.cursor = 3;
    let effects = apply_command(&mut st, &Command::Delete(1, Motion::Tag { around: false }));
    assert!(!effects.contains(&Effect::Beep));
    assert_eq!(st.doc.bytes(), b"<a></a>");
}
