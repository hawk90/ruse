//! `:s`, its confirm loop and case modifiers, and `:g`. Split from `editor/tests.rs` (#1397).

use crate::editor::*;

fn sub(
    initial: &str,
    range: SubRange,
    pat: &str,
    rep: &str,
    flags: SubFlags,
) -> (EditorState, SubOutcome) {
    let mut st = EditorState::new(initial.as_bytes().to_vec());
    let out = st.substitute(range, pat, rep, flags).expect("compiles");
    (st, out)
}

/// F-009 #2: `:s/pat/rep/` replaces the FIRST match on the current line only.
#[test]
fn substitute_first_match_on_current_line() {
    let (st, out) = sub(
        "foo foo\nfoo",
        SubRange::CurrentLine,
        "foo",
        "bar",
        SubFlags::default(),
    );
    assert_eq!(st.as_str().unwrap(), "bar foo\nfoo");
    assert_eq!((out.replacements, out.lines), (1, 1));
}

/// #692 review: `:%s/^/#/` on "abc\ndef\n" prefixes the two lines and adds no "#" line for the
/// empty slot after the final newline (nvim: "#abc\n#def\n", 2 substitutions). An empty buffer is
/// still one (empty) line.
#[test]
fn substitute_skips_the_slot_after_the_final_newline() {
    let (st, out) = sub(
        "abc\ndef\n",
        SubRange::WholeFile,
        "^",
        "#",
        SubFlags::default(),
    );
    assert_eq!(st.as_str().unwrap(), "#abc\n#def\n");
    assert_eq!((out.replacements, out.lines), (2, 2));
    let (st, _) = sub("abc\n", SubRange::WholeFile, "$", ";", SubFlags::default());
    assert_eq!(st.as_str().unwrap(), "abc;\n");
    let (st, _) = sub("", SubRange::WholeFile, "^", "#", SubFlags::default());
    assert_eq!(st.as_str().unwrap(), "#\n");
}

/// #715: a `\n` in the pattern matches the line break, across lines, as in nvim v0.12.6 — with and
/// without a final newline, and with Vim's "go on in the joined line" rule (`do_again`) bounded by the
/// range.
#[test]
fn substitute_matches_line_breaks() {
    let none = SubFlags::default();
    let g = SubFlags {
        global: true,
        ignore_case: None,
    };
    let whole = SubRange::WholeFile;
    let cases: &[(&str, SubRange, &str, &str, SubFlags, &str)] = &[
        ("a\nb\n", whole, r"\n", ",", none, "a,b,\n"),
        ("a\nb", whole, r"\n", ",", none, "a,b,"),
        ("a\nb\nc", SubRange::Lines(1, 1), r"\n", ",", none, "a,b\nc"),
        ("a\nb\nc", SubRange::Lines(1, 2), r"\n", ",", none, "a,b,c"),
        ("a\nb\nc\nd", whole, r"b\nc", "X", none, "a\nX\nd"),
        ("ab\nab\nc", whole, r"b\na", "X", g, "aXb\nc"),
        ("a\n\n\nb", whole, r"\n\n", "X", none, "aX\nb"),
        ("a b\nc", whole, r" \|\n", "_", g, "a_b_c_"),
        ("a\nb", whole, r"$\n", ",", none, "a,b,"),
        ("a\nb", whole, r"a\_.*", "X", none, "X"),
        ("a\nb", whole, r"\n\zs", "X", none, "a\nXb"),
        ("a\nb", whole, r"a\ze\nb", "X", none, "X\nb"),
        ("a\nb", whole, r"\n", "&&", none, "a\n\nb\n\n"),
        ("aa\nb", whole, r"a*\n", "X", g, "XbX"),
        ("a\nb\nc", whole, r"\n", r"\r\r", none, "a\n\nb\n\nc\n\n"),
    ];
    for &(text, range, pat, rep, flags, want) in cases {
        let (st, _) = sub(text, range, pat, rep, flags);
        assert_eq!(st.as_str().unwrap(), want, "{text:?} :s/{pat}/{rep}/");
    }
    let (_, out) = sub("a\nb\nc", whole, r"\n", ",", none);
    assert_eq!(
        (out.replacements, out.lines),
        (3, 1),
        "one line, as Vim counts"
    );
}

/// #715: `:s/\n//` is Vim's join (`sub_joining_lines`): the range plus the next line, the cursor on the
/// last seam; on the last line it has nothing to join and is not a miss.
#[test]
fn substitute_newline_with_nothing_is_a_join() {
    let none = SubFlags::default();
    let (st, out) = sub("ab\ncd\nef", SubRange::WholeFile, r"\n", "", none);
    assert_eq!(st.as_str().unwrap(), "abcdef");
    assert_eq!(st.cursor().0, 4);
    assert_eq!((out.replacements, out.lines), (2, 1));
    let (st, _) = sub("ab\ncd\nef", SubRange::Lines(1, 2), r"\n", "", none);
    assert_eq!(st.as_str().unwrap(), "abcdef");
    let (st, out) = sub("ab\ncd", SubRange::Lines(2, 2), r"\n", "", none);
    assert_eq!(st.as_str().unwrap(), "ab\ncd");
    assert_eq!((out.replacements, out.lines), (0, 1));
}

/// #698: `\r` splits the line and the cursor follows to the line the last replacement ends on, at its
/// first non-blank (nvim: `x\n  abc` `:%s/b/1\r  2/` → cursor on `  2c`, column 2).
#[test]
fn substitute_line_break_moves_the_cursor_down() {
    let (st, _) = sub(
        "x\n  abc",
        SubRange::WholeFile,
        "b",
        r"1\r  2",
        SubFlags::default(),
    );
    assert_eq!(st.as_str().unwrap(), "x\n  a1\n  2c");
    assert_eq!(st.cursor().0, "x\n  a1\n  ".len());
    let (st, _) = sub(
        "a,b",
        SubRange::CurrentLine,
        ",",
        r"\n",
        SubFlags::default(),
    );
    assert_eq!(st.doc.bytes(), b"a\0b");
}

/// F-009 #2: the `g` flag replaces ALL matches on each line.
#[test]
fn substitute_global_flag() {
    let flags = SubFlags {
        global: true,
        ignore_case: None,
    };
    let (st, out) = sub("foo foo foo", SubRange::CurrentLine, "foo", "X", flags);
    assert_eq!(st.as_str().unwrap(), "X X X");
    assert_eq!(out.replacements, 3);
}

/// F-009 #2: `:%s///g` spans the whole file as ONE undo group.
#[test]
fn substitute_whole_file_is_one_undo_group() {
    let flags = SubFlags {
        global: true,
        ignore_case: None,
    };
    let (mut st, _) = sub("a a\nb a\na", SubRange::WholeFile, "a", "Z", flags);
    assert_eq!(st.as_str().unwrap(), "Z Z\nb Z\nZ");
    // A single `u` reverts the ENTIRE substitution (it was one transaction).
    apply_command(&mut st, &Command::Undo);
    assert_eq!(st.as_str().unwrap(), "a a\nb a\na");
}

/// F-009 #2: a `:N,Ms` line range restricts the substitution.
#[test]
fn substitute_line_range() {
    let flags = SubFlags {
        global: true,
        ignore_case: None,
    };
    let (st, _) = sub("a\na\na\na", SubRange::Lines(2, 3), "a", "X", flags);
    assert_eq!(st.as_str().unwrap(), "a\nX\nX\na");
}

/// F-009 #2: `&` in the replacement is the whole matched text (`:s/foo/[&]/`).
#[test]
fn ampersand_is_the_whole_match() {
    let (st, _) = sub(
        "say foo",
        SubRange::CurrentLine,
        "foo",
        "[&]",
        SubFlags::default(),
    );
    assert_eq!(st.as_str().unwrap(), "say [foo]");
}

/// F-009 #3: `:s/foo\zsbar/X/` rewrites only the reported (`\zs`) span — `foo` is kept.
#[test]
fn substitute_honors_zs_reported_span() {
    let (st, _) = sub(
        "a foobar b",
        SubRange::CurrentLine,
        "foo\\zsbar",
        "X",
        SubFlags::default(),
    );
    assert_eq!(st.as_str().unwrap(), "a fooX b");
}

/// F-009 #2: the `i` flag forces a case-insensitive substitution regardless of config.
#[test]
fn substitute_i_flag_ignores_case() {
    let flags = SubFlags {
        global: true,
        ignore_case: Some(true),
    };
    let (st, _) = sub("Foo FOO foo", SubRange::CurrentLine, "foo", "x", flags);
    assert_eq!(st.as_str().unwrap(), "x x x");
}

#[cfg(test)]
mod substitute_confirm_tests {
    use crate::editor::*;

    /// F-009 #2 (`:s///c`): preview finds all candidate matches WITHOUT editing; applying only an
    /// ACCEPTED subset (as the confirm loop does on y/n) replaces just those, in one undo group.
    #[test]
    fn preview_then_apply_accepted_subset() {
        let flags = SubFlags {
            global: true,
            ignore_case: None,
        };
        let mut st = EditorState::new(b"a a a".to_vec());
        let subs = st
            .substitute_preview(SubRange::CurrentLine, "a", "X", flags)
            .expect("compiles");
        assert_eq!(subs.len(), 3, "three candidate matches");
        assert_eq!(st.as_str().unwrap(), "a a a", "preview does not edit");

        // Accept the first and last, skip the middle (as `y n y` would).
        let accepted = vec![subs[0].clone(), subs[2].clone()];
        let out = st.apply_substitutions(&accepted);
        assert_eq!(st.as_str().unwrap(), "X a X");
        assert_eq!(out.replacements, 2);
        // One `u` reverts BOTH accepted substitutions (they were one transaction).
        apply_command(&mut st, &Command::Undo);
        assert_eq!(st.as_str().unwrap(), "a a a");
    }

    /// Applying an EMPTY accepted set (the confirm loop when the user answered `n` to everything or
    /// quit immediately) is a clean no-op.
    #[test]
    fn apply_no_substitutions_is_a_noop() {
        let mut st = EditorState::new(b"a a".to_vec());
        let out = st.apply_substitutions(&[]);
        assert_eq!((out.replacements, out.lines), (0, 0));
        assert_eq!(st.as_str().unwrap(), "a a");
    }
}

#[cfg(test)]
mod substitute_case_modifier_tests {
    //! `:s` replacement case modifiers `\u \l \U \L \e \E`. Each expected value is the exact output
    //! produced by nvim v0.12.4 (`nvim -u NONE` headless) for the same pattern/replacement, so these
    //! lock parity. Capture backrefs `\1`-`\9` are deferred (see `expand_replacement` docs) — not tested.
    use crate::editor::substitute::expand_replacement;
    use crate::editor::*;

    /// Assert `expand_replacement(rep, matched)` yields exactly `want` (as bytes).
    fn ex(rep: &str, matched: &str, want: &str) {
        let got = expand_replacement(rep, matched);
        assert_eq!(
            String::from_utf8(got.clone()).unwrap_or_else(|_| format!("{got:?}")),
            want,
            "expand_replacement({rep:?}, {matched:?})"
        );
    }

    /// `\u&` uppercases the first char of the inserted match; the rest is untouched.
    /// nvim: `:s/\w\+/\u&/g` on "hello world" → "Hello World".
    #[test]
    fn upper_next_char_capitalizes_match() {
        ex(r"\u&", "hello", "Hello");
        ex(r"\u&", "world", "World");
    }

    /// `\U&` uppercases the whole inserted match (region, no `\E`). nvim: `:s/.*/\U&/` "aBcD" → "ABCD".
    #[test]
    fn upper_region_uppercases_whole_match() {
        ex(r"\U&", "aBcD", "ABCD");
        ex(r"\U&", "hello world", "HELLO WORLD");
    }

    /// `\L&` lowercases the whole inserted match. nvim: `:s/.*/\L&/` "HELLO WORLD" → "hello world".
    #[test]
    fn lower_region_lowercases_whole_match() {
        ex(r"\L&", "HELLO WORLD", "hello world");
    }

    /// `\l` lowercases only the next char. nvim: `:s/.*/\l&/` "HELLO" → "hELLO".
    #[test]
    fn lower_next_char_only() {
        ex(r"\l&", "HELLO", "hELLO");
    }

    /// `\u` before literal text capitalizes the literal, not the match. nvim: `:s/.*/\uabc&/` "hello"
    /// → "Abchello" (the `A` is the uppercased literal `a`; the match is inserted as-is after).
    #[test]
    fn upper_next_char_applies_to_literal() {
        ex(r"\uabc&", "hello", "Abchello");
    }

    /// A `\U…\E` region ends at `\E`; text after `\E` is emitted verbatim. nvim: `:s/.*/\Uabc\Edef&/`
    /// "hello world" → "ABCdefhello world".
    #[test]
    fn upper_region_ends_at_capital_e() {
        ex(r"\Uabc\Edef&", "hello world", "ABCdefhello world");
        // Region with the match inside, then a literal tail after `\E`.
        ex(r"\U&\Etail", "abcdef", "ABCDEFtail");
    }

    /// `\e` (lowercase) also ends a region. nvim: `:s/.*/\Ux\ey&/` "ab" → "Xyab".
    #[test]
    fn lowercase_e_ends_region() {
        ex(r"\Ux\ey&", "ab", "Xyab");
    }

    /// The region composes with a line break: it stays active across the `\r` that splits the line, so
    /// both `&` insertions are uppercased. `\n` is a NUL byte, as in Vim (#698).
    #[test]
    fn region_composes_with_newline_escape() {
        ex(r"\U&\r&", "ab", "AB\nAB");
        ex(r"\U&\n&", "ab", "AB\0AB");
    }

    /// #698 (nvim v0.12.6): `\r` and a literal carriage return split the line, `\<CR>` inserts a
    /// carriage return, `\n` inserts NUL, and a carriage return in the matched text stays one.
    #[test]
    fn line_break_escapes_follow_vim() {
        ex(r"a\rb", "", "a\nb");
        ex("a\rb", "", "a\nb");
        ex("a\\\rb", "", "a\rb");
        ex(r"a\nb", "", "a\0b");
        ex(r"\\r", "", "\\r");
        ex("&", "x\ry", "x\ry");
    }

    /// A pending one-char modifier wins over an active region for exactly one char, then the region
    /// resumes. nvim: `:s/.*/\l\U&/` "abcd" → "aBCD" (pending `\l` lowers `a`, region `\U` upcases rest).
    #[test]
    fn pending_one_char_overrides_region_then_region_resumes() {
        ex(r"\l\U&", "abcd", "aBCD");
        // Symmetric: `\U\l&` on "ABCD" → "aBCD".
        ex(r"\U\l&", "ABCD", "aBCD");
        // `\L\u&` on "abcd" → "Abcd".
        ex(r"\L\u&", "abcd", "Abcd");
    }

    /// A pending modifier is consumed by the next emitted char even mid-string, so only the first char
    /// of the match is affected. nvim: `:s/.*/&\utail/` "abc" → "abcTail".
    #[test]
    fn pending_after_match_capitalizes_following_literal() {
        ex(r"&\utail", "abc", "abcTail");
    }

    /// `\0` behaves like `&` and honors case modifiers.
    #[test]
    fn backslash_zero_is_whole_match_and_cased() {
        ex(r"\U\0", "abc", "ABC");
    }

    /// Unsupported `\<digit>` (backrefs are deferred) keeps the digit literal, and case modifiers still
    /// apply to it as an emitted char.
    #[test]
    fn deferred_backref_digit_stays_literal() {
        ex(r"\1", "abc", "1");
        ex(r"x\1y", "m", "x1y");
    }

    /// End-to-end through `EditorState::substitute`: `\u&` capitalizes each word with the `g` flag.
    #[test]
    fn end_to_end_capitalize_each_word() {
        let mut st = EditorState::new(b"abc def".to_vec());
        let flags = SubFlags {
            global: true,
            ignore_case: None,
        };
        st.substitute(SubRange::CurrentLine, r"\w\+", r"\u&", flags)
            .expect("compiles");
        assert_eq!(st.as_str().unwrap(), "Abc Def");
    }

    /// End-to-end: `\U&` uppercases the whole line match.
    #[test]
    fn end_to_end_upper_whole_line() {
        let mut st = EditorState::new(b"hello world".to_vec());
        st.substitute(SubRange::CurrentLine, r".*", r"\U&", SubFlags::default())
            .expect("compiles");
        assert_eq!(st.as_str().unwrap(), "HELLO WORLD");
    }
}

#[cfg(test)]
mod global_tests {
    use crate::editor::*;

    fn run_global(initial: &str, pat: &str, negate: bool, cmd: GlobalCmd) -> EditorState {
        let mut st = EditorState::new(initial.as_bytes().to_vec());
        st.global(SubRange::WholeFile, pat, negate, &cmd)
            .expect("compiles");
        st
    }

    /// F-009 #4: `:g/pat/d` deletes every line matching the pattern (two-pass: mark then execute).
    #[test]
    fn global_delete_matching_lines() {
        let st = run_global(
            "keep\ndrop me\nkeep2\ndrop\n",
            "drop",
            false,
            GlobalCmd::Delete,
        );
        assert_eq!(st.as_str().unwrap(), "keep\nkeep2\n");
    }

    /// #1100 (nvim v0.12.6): `:g/a/d` deletes line by line, so `""` and `"1` hold the last deleted line and
    /// the earlier ones shift down the numbered ring.
    #[test]
    fn global_delete_fills_the_unnamed_and_numbered_registers() {
        let st = run_global("a1\nb\na2\nc\na3", "a", false, GlobalCmd::Delete);
        assert_eq!(st.as_str().unwrap(), "b\nc");
        let reg = |r| st.view.registers.get(r).text().to_vec();
        assert_eq!(reg(None), b"a3\n");
        assert!(st.view.registers.get(None).is_linewise());
        assert_eq!(reg(Some('1')), b"a3\n");
        assert_eq!(reg(Some('2')), b"a2\n");
        assert_eq!(reg(Some('3')), b"a1\n");
    }

    /// F-009 #4: `:g!/pat/d` (== `:v/pat/d`) deletes the NON-matching lines.
    #[test]
    fn global_negate_deletes_non_matching() {
        let st = run_global("a1\nb\na2\nc\n", "a", true, GlobalCmd::Delete);
        assert_eq!(st.as_str().unwrap(), "a1\na2\n");
    }

    /// #715: a multi-line `:g` pattern marks the line its match starts on (nvim: `:g/a\nb/d` on a/b/c
    /// leaves b/c), and a `:s` inside `:g` skips a marked line an earlier match joined away.
    #[test]
    fn global_multiline_pattern_marks_the_starting_line() {
        let st = run_global("a\nb\nc", r"a\nb", false, GlobalCmd::Delete);
        assert_eq!(st.as_str().unwrap(), "b\nc");
        let st = run_global("x\nx\nx\ny", r"x\nx", true, GlobalCmd::Delete);
        assert_eq!(st.as_str().unwrap(), "x\nx");
        let st = run_global(
            "a\na\nb",
            "a",
            false,
            GlobalCmd::Substitute {
                pattern: r"\n".into(),
                replacement: "-".into(),
                flags: SubFlags::default(),
            },
        );
        assert_eq!(st.as_str().unwrap(), "a-a\nb");
    }

    /// F-009 #4: the two-pass mark-then-execute means a `:g/pat/s///` only substitutes on lines that
    /// matched the `:g` selector, and marking is unaffected by the substitutions.
    #[test]
    fn global_substitute_on_matching_lines_only() {
        let st = run_global(
            "foo x\nbar x\nfoo x\n",
            "foo",
            false,
            GlobalCmd::Substitute {
                pattern: "x".into(),
                replacement: "Y".into(),
                flags: SubFlags {
                    global: true,
                    ignore_case: None,
                },
            },
        );
        // Only the two "foo" lines get their x -> Y; the "bar" line is untouched.
        assert_eq!(st.as_str().unwrap(), "foo Y\nbar x\nfoo Y\n");
    }

    /// F-009 #4: the whole `:g` is ONE undo group.
    #[test]
    fn global_delete_is_one_undo_group() {
        let mut st = EditorState::new(b"a\ndrop\nb\ndrop\nc\n".to_vec());
        st.global(SubRange::WholeFile, "drop", false, &GlobalCmd::Delete)
            .unwrap();
        assert_eq!(st.as_str().unwrap(), "a\nb\nc\n");
        apply_command(&mut st, &Command::Undo);
        assert_eq!(st.as_str().unwrap(), "a\ndrop\nb\ndrop\nc\n");
    }
}
