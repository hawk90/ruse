#[cfg(test)]
mod tests {
    use crate::input::*;

    pub(super) fn k(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    /// A Meta (Alt) key event — the shared helper for the Emacs-tier tests (`M-f`, `M-d`, …).
    pub(super) fn meta(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
    }
    pub(super) fn feed(seq: &str) -> Feed {
        let mut e = InputEngine::new();
        let mut last = Feed::Ignored;
        for c in seq.chars() {
            last = e.feed(k(c), Mode::Normal);
        }
        last
    }

    #[test]
    fn bare_motions_and_counts() {
        assert_eq!(feed("w"), Feed::Cmd(Command::Move(1, Motion::WordFwd)));
        assert_eq!(feed("3w"), Feed::Cmd(Command::Move(3, Motion::WordFwd)));
        assert_eq!(feed("l"), Feed::Cmd(Command::Move(1, Motion::Right)));
    }

    #[test]
    fn substitute_char_s_is_change_right() {
        // `s` = `cl`: change one char rightward; `3s` changes three. `cl` produces the same command.
        assert_eq!(feed("s"), Feed::Cmd(Command::Change(1, Motion::Right)));
        assert_eq!(feed("3s"), Feed::Cmd(Command::Change(3, Motion::Right)));
        assert_eq!(feed("cl"), Feed::Cmd(Command::Change(1, Motion::Right)));
    }

    #[test]
    fn g_underscore_and_pipe_motions() {
        assert_eq!(
            feed("g_"),
            Feed::Cmd(Command::Move(1, Motion::LineLastNonBlank))
        );
        assert_eq!(
            feed("dg_"),
            Feed::Cmd(Command::Delete(1, Motion::LineLastNonBlank))
        );
        assert_eq!(feed("|"), Feed::Cmd(Command::Move(1, Motion::Column)));
        assert_eq!(feed("5|"), Feed::Cmd(Command::Move(5, Motion::Column)));
        assert_eq!(feed("d5|"), Feed::Cmd(Command::Delete(5, Motion::Column)));
    }

    #[test]
    fn g_prefixed_display_line_motions_alias_plain_motions() {
        // ruse does not soft-wrap (one buffer line == one display row), so Vim's display-line motions
        // `gj`/`gk`/`g0`/`g$`/`g^` equal `j`/`k`/`0`/`$`/`^` — exactly as Vim itself under `nowrap`.
        assert_eq!(feed("gj"), Feed::Cmd(Command::Move(1, Motion::Down)));
        assert_eq!(feed("gk"), Feed::Cmd(Command::Move(1, Motion::Up)));
        assert_eq!(feed("g0"), Feed::Cmd(Command::Move(1, Motion::LineStart)));
        assert_eq!(feed("g$"), Feed::Cmd(Command::Move(1, Motion::LineEnd)));
        assert_eq!(
            feed("g^"),
            Feed::Cmd(Command::Move(1, Motion::LineFirstNonBlank))
        );
        // Count-aware: the count accumulated before `g` carries through (`3gj` == `3j`).
        assert_eq!(feed("3gj"), Feed::Cmd(Command::Move(3, Motion::Down)));
        assert_eq!(feed("2gk"), Feed::Cmd(Command::Move(2, Motion::Up)));
        // Operator-composable. The horizontal forms match nvim exactly (`dg$` == `d$`, `dg^` == `d^`).
        assert_eq!(feed("g$"), feed("$"));
        assert_eq!(feed("dg$"), feed("d$"));
        assert_eq!(feed("dg^"), feed("d^"));
        assert_eq!(feed("dg0"), feed("d0"));
        // DELIBERATE DIVERGENCE (operator over the VERTICAL forms): `dgj`/`dgk` alias `dj`/`dk` (linewise).
        // nvim treats `gj`/`gk` as characterwise-exclusive (subject to exclusive-linewise promotion), so
        // nvim's `dgj` deletes ONE line, not two — see the note in input/mod.rs's GSecond arms.
        assert_eq!(feed("dgj"), Feed::Cmd(Command::Delete(1, Motion::Down)));
        assert_eq!(feed("ygk"), Feed::Cmd(Command::Yank(1, Motion::Up)));
        assert_eq!(feed("d2gj"), Feed::Cmd(Command::Delete(2, Motion::Down)));
    }

    #[test]
    fn plus_minus_underscore_line_motions() {
        // `+` / `-` / `_` — first-non-blank line motions, operator-aware and linewise.
        assert_eq!(
            feed("+"),
            Feed::Cmd(Command::Move(1, Motion::DownFirstNonBlank))
        );
        assert_eq!(
            feed("2+"),
            Feed::Cmd(Command::Move(2, Motion::DownFirstNonBlank))
        );
        assert_eq!(
            feed("-"),
            Feed::Cmd(Command::Move(1, Motion::UpFirstNonBlank))
        );
        assert_eq!(
            feed("3-"),
            Feed::Cmd(Command::Move(3, Motion::UpFirstNonBlank))
        );
        assert_eq!(
            feed("_"),
            Feed::Cmd(Command::Move(1, Motion::LineUnderscore))
        );
        assert_eq!(
            feed("d+"),
            Feed::Cmd(Command::Delete(1, Motion::DownFirstNonBlank))
        );
        assert_eq!(
            feed("d_"),
            Feed::Cmd(Command::Delete(1, Motion::LineUnderscore))
        );
        assert_eq!(
            feed("2d-"),
            Feed::Cmd(Command::Delete(2, Motion::UpFirstNonBlank))
        );
        // `<CR>` is a synonym for `+` (KeyCode::Enter, not a Char, so drive the engine directly).
        let mut e = InputEngine::new();
        assert_eq!(
            e.feed(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                Mode::Normal
            ),
            Feed::Cmd(Command::Move(1, Motion::DownFirstNonBlank))
        );
    }

    #[test]
    fn context_mark_jumps_backtick_and_quote() {
        // `` `` `` → exact context mark; `''` → linewise. Both accept the reversed second key too.
        assert_eq!(feed("``"), Feed::Cmd(Command::GotoContextMark));
        assert_eq!(feed("`'"), Feed::Cmd(Command::GotoContextMark));
        assert_eq!(feed("''"), Feed::Cmd(Command::GotoContextMarkLine));
        assert_eq!(feed("'`"), Feed::Cmd(Command::GotoContextMarkLine));
    }

    #[test]
    fn goto_byte_motion() {
        // `[count]go` — go to the count-th byte; bare `go` = byte 1; operator-aware `dgo`.
        assert_eq!(feed("go"), Feed::Cmd(Command::Move(1, Motion::GotoByte)));
        assert_eq!(feed("12go"), Feed::Cmd(Command::Move(12, Motion::GotoByte)));
        assert_eq!(
            feed("d5go"),
            Feed::Cmd(Command::Delete(5, Motion::GotoByte))
        );
    }

    #[test]
    fn tag_text_objects() {
        assert_eq!(
            feed("dit"),
            Feed::Cmd(Command::Delete(1, Motion::Tag { around: false }))
        );
        assert_eq!(
            feed("dat"),
            Feed::Cmd(Command::Delete(1, Motion::Tag { around: true }))
        );
        assert_eq!(
            feed("cit"),
            Feed::Cmd(Command::Change(1, Motion::Tag { around: false }))
        );
        assert_eq!(
            feed("yat"),
            Feed::Cmd(Command::Yank(1, Motion::Tag { around: true }))
        );
    }

    #[test]
    fn star_hash_search_word_under_cursor() {
        assert_eq!(
            feed("*"),
            Feed::Cmd(Command::SearchWordUnder {
                forward: true,
                whole_word: true
            })
        );
        assert_eq!(
            feed("#"),
            Feed::Cmd(Command::SearchWordUnder {
                forward: false,
                whole_word: true
            })
        );
        // `g*` / `g#` — match anywhere (no word boundaries).
        assert_eq!(
            feed("g*"),
            Feed::Cmd(Command::SearchWordUnder {
                forward: true,
                whole_word: false
            })
        );
        assert_eq!(
            feed("g#"),
            Feed::Cmd(Command::SearchWordUnder {
                forward: false,
                whole_word: false
            })
        );
    }

    #[test]
    fn gd_and_gd_global_emit_goto_declaration() {
        // `gd` (local) and `gD` (global) both go through the GSecond tier; the frontend rewrites them to a
        // concrete whole-file first-match jump. `global` distinguishes them (currently identical behavior).
        assert_eq!(
            feed("gd"),
            Feed::Cmd(Command::GotoDeclaration { global: false })
        );
        assert_eq!(
            feed("gD"),
            Feed::Cmd(Command::GotoDeclaration { global: true })
        );
    }

    #[test]
    fn case_operators_g_prefix() {
        use ruse_core::WordCase;
        // gu/gU/g~ over a motion.
        assert_eq!(
            feed("guw"),
            Feed::Cmd(Command::CaseMotion {
                count: 1,
                motion: Motion::WordFwd,
                case: WordCase::Downcase
            })
        );
        assert_eq!(
            feed("gU$"),
            Feed::Cmd(Command::CaseMotion {
                count: 1,
                motion: Motion::LineEnd,
                case: WordCase::Upcase
            })
        );
        assert_eq!(
            feed("g~w"),
            Feed::Cmd(Command::CaseMotion {
                count: 1,
                motion: Motion::WordFwd,
                case: WordCase::Toggle
            })
        );
        // Doubled → linewise.
        assert_eq!(
            feed("guu"),
            Feed::Cmd(Command::CaseMotion {
                count: 1,
                motion: Motion::Line,
                case: WordCase::Downcase
            })
        );
        assert_eq!(
            feed("gUU"),
            Feed::Cmd(Command::CaseMotion {
                count: 1,
                motion: Motion::Line,
                case: WordCase::Upcase
            })
        );
        assert_eq!(
            feed("g~~"),
            Feed::Cmd(Command::CaseMotion {
                count: 1,
                motion: Motion::Line,
                case: WordCase::Toggle
            })
        );
        // Count folds through: 2gUw.
        assert_eq!(
            feed("2gUw"),
            Feed::Cmd(Command::CaseMotion {
                count: 2,
                motion: Motion::WordFwd,
                case: WordCase::Upcase
            })
        );
    }

    #[test]
    fn sentence_motions_parens() {
        assert_eq!(feed(")"), Feed::Cmd(Command::Move(1, Motion::SentenceFwd)));
        assert_eq!(feed("("), Feed::Cmd(Command::Move(1, Motion::SentenceBack)));
        assert_eq!(
            feed("d)"),
            Feed::Cmd(Command::Delete(1, Motion::SentenceFwd))
        );
        assert_eq!(
            feed("2("),
            Feed::Cmd(Command::Move(2, Motion::SentenceBack))
        );
    }

    #[test]
    fn backward_word_end_g_prefix() {
        // `ge`/`gE` move; the count before `g` carries through; `dge` is an operator over the motion.
        assert_eq!(feed("ge"), Feed::Cmd(Command::Move(1, Motion::WordEndBack)));
        assert_eq!(
            feed("gE"),
            Feed::Cmd(Command::Move(1, Motion::BigWordEndBack))
        );
        assert_eq!(
            feed("2ge"),
            Feed::Cmd(Command::Move(2, Motion::WordEndBack))
        );
        assert_eq!(
            feed("dge"),
            Feed::Cmd(Command::Delete(1, Motion::WordEndBack))
        );
    }

    #[test]
    fn operators_with_counts() {
        assert_eq!(feed("dw"), Feed::Cmd(Command::Delete(1, Motion::WordFwd)));
        assert_eq!(feed("d2w"), Feed::Cmd(Command::Delete(2, Motion::WordFwd)));
        assert_eq!(feed("2dw"), Feed::Cmd(Command::Delete(2, Motion::WordFwd)));
        assert_eq!(feed("2d3w"), Feed::Cmd(Command::Delete(6, Motion::WordFwd)));
    }

    // --- F-027 Lang-Arg translation stage (D-048 / RFC-0013) ---------------------------------------

    fn ctrl_caret() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('^'), KeyModifiers::CONTROL)
    }

    #[test]
    fn lang_stage_translates_in_insert_one_substitution() {
        // Acceptance #1 + INV-FAIL-BOUNDED: a mapped key is rewritten exactly ONCE and dispatched
        // literally. A cyclic map (a<->b) cannot loop — `a` becomes `b`, never re-translated back to `a`.
        let mut e = InputEngine::new();
        e.lang_map.insert('a', 'b');
        e.lang_map.insert('b', 'a');
        e.lang_active = true;
        assert_eq!(
            e.feed(k('a'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('b'))
        );
        assert_eq!(
            e.feed(k('b'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('a'))
        );
    }

    #[test]
    fn lang_stage_inert_outside_the_three_contexts() {
        // Acceptance #2 ("and to nothing else"): Normal/Replace never translate — operators and motions
        // are immune. Map `d`->`w`; in Normal `d` still ARMS the delete operator (Pending), it is NOT
        // rewritten to the `w` motion.
        let mut e = InputEngine::new();
        e.lang_map.insert('d', 'w');
        e.lang_active = true;
        assert_eq!(
            e.translate_lang(k('d'), Mode::Normal).code,
            KeyCode::Char('d')
        );
        assert_eq!(e.feed(k('d'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.translate_lang(k('d'), Mode::Replace).code,
            KeyCode::Char('d')
        );
    }

    #[test]
    fn lang_stage_translates_single_char_argument() {
        // Acceptance #2 (positive half): a command reading a single character (`f`/`r`) has its ARGUMENT
        // translated, regardless of how the command was reached.
        let mut e = InputEngine::new();
        e.lang_map.insert('a', 'x');
        e.lang_active = true;
        // `f a` finds the TRANSLATED char `x`.
        assert_eq!(e.feed(k('f'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.feed(k('a'), Mode::Normal),
            Feed::Cmd(Command::Move(
                1,
                Motion::FindChar {
                    ch: 'x',
                    forward: true,
                    till: false
                }
            ))
        );
        // `r a` replaces with the TRANSLATED char `x`.
        assert_eq!(e.feed(k('r'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.feed(k('a'), Mode::Normal),
            Feed::Cmd(Command::ReplaceChar(1, 'x'))
        );
    }

    #[test]
    fn lang_stage_off_by_default_and_toggled_by_ctrl_caret() {
        // The map is inert until activated (so `:lmap`-defining keystrokes are never rewritten), and
        // `i_CTRL-^` toggles it within Insert (RFC-0013).
        let mut e = InputEngine::new();
        e.lang_map.insert('a', 'б');
        // Off by default: `a` inserts `a`.
        assert_eq!(
            e.feed(k('a'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('a'))
        );
        // CTRL-^ activates (a silent, non-inserting toggle).
        assert_eq!(e.feed(ctrl_caret(), Mode::Insert), Feed::Pending);
        assert_eq!(
            e.feed(k('a'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('б'))
        );
        // CTRL-^ again deactivates.
        assert_eq!(e.feed(ctrl_caret(), Mode::Insert), Feed::Pending);
        assert_eq!(
            e.feed(k('a'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('a'))
        );
    }

    #[test]
    fn lang_stage_translates_on_the_command_line() {
        // The Command-line namespace is Lang-Arg-eligible: a mapped key appends its TRANSLATION to the
        // owned line buffer (F-026 owns the buffer; the stage rewrites the key before it lands there).
        let mut e = InputEngine::new();
        e.lang_map.insert('a', 'б');
        e.lang_active = true;
        assert_eq!(e.feed(k(':'), Mode::Normal), Feed::Pending); // opens the command line
        let _ = e.feed(k('a'), Mode::Normal);
        assert_eq!(e.cmdline.as_ref().map(|c| c.buffer.as_str()), Some("б"));
    }

    #[test]
    fn lang_map_maintenance_methods() {
        // `set_lang_mapping` / `clear_lang_mapping` (the `:lmap` / `:lunmap` back-ends).
        let mut e = InputEngine::new();
        e.set_lang_mapping('a', 'б');
        e.lang_active = true;
        assert_eq!(
            e.feed(k('a'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('б'))
        );
        e.clear_lang_mapping('a');
        assert_eq!(
            e.feed(k('a'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('a'))
        );
    }

    #[test]
    fn parse_lmap_and_lunmap() {
        assert_eq!(
            parse_ex("lmap a б"),
            Ex::Lmap {
                lhs: 'a', rhs: 'б'
            }
        );
        assert_eq!(parse_ex("lunmap a"), Ex::Lunmap { lhs: 'a' });
        // A verb needs whitespace, both sides are single chars, and no trailing tokens — else Unknown.
        assert!(matches!(parse_ex("lmapx"), Ex::Unknown(_)));
        assert!(matches!(parse_ex("lmap a"), Ex::Unknown(_)));
        assert!(matches!(parse_ex("lmap ab cd"), Ex::Unknown(_)));
        assert!(matches!(parse_ex("lmap a b c"), Ex::Unknown(_)));
        assert!(matches!(parse_ex("lunmapx"), Ex::Unknown(_)));
    }

    #[test]
    fn parse_quit_and_quit_bang() {
        assert_eq!(parse_ex("q"), Ex::Quit);
        assert_eq!(parse_ex("quit"), Ex::Quit);
        assert_eq!(parse_ex("q!"), Ex::QuitForce);
        assert_eq!(parse_ex("quit!"), Ex::QuitForce);
    }

    #[test]
    fn parse_only() {
        assert_eq!(parse_ex("only"), Ex::Only);
        assert_eq!(parse_ex("on"), Ex::Only);
    }

    #[test]
    fn parse_delete_ranges() {
        use ruse_core::SubRange;
        assert_eq!(parse_ex("d"), Ex::Delete(SubRange::CurrentLine));
        assert_eq!(parse_ex("delete"), Ex::Delete(SubRange::CurrentLine));
        assert_eq!(parse_ex("2,5d"), Ex::Delete(SubRange::Lines(2, 5)));
        assert_eq!(parse_ex("%d"), Ex::Delete(SubRange::WholeFile));
        assert_eq!(parse_ex("3delete"), Ex::Delete(SubRange::Lines(3, 3)));
        // Not a delete verb → falls through (Unknown), not a spurious Delete.
        assert!(matches!(parse_ex("diffthis"), Ex::Unknown(_)));
    }

    #[test]
    fn parse_yank_ranges() {
        use ruse_core::SubRange;
        assert_eq!(parse_ex("y"), Ex::Yank(SubRange::CurrentLine));
        assert_eq!(parse_ex("yank"), Ex::Yank(SubRange::CurrentLine));
        assert_eq!(parse_ex("1,3y"), Ex::Yank(SubRange::Lines(1, 3)));
        assert_eq!(parse_ex("%yank"), Ex::Yank(SubRange::WholeFile));
    }

    #[test]
    fn parse_move_and_copy() {
        use ruse_core::{LineAddr, SubRange};
        assert_eq!(
            parse_ex("3,5m10"),
            Ex::Move(SubRange::Lines(3, 5), LineAddr::Line(10))
        );
        assert_eq!(
            parse_ex("m0"),
            Ex::Move(SubRange::CurrentLine, LineAddr::Line(0))
        );
        assert_eq!(
            parse_ex("move$"),
            Ex::Move(SubRange::CurrentLine, LineAddr::Last)
        );
        assert_eq!(
            parse_ex(".m$"),
            Ex::Move(SubRange::CurrentLine, LineAddr::Last)
        );
        assert_eq!(
            parse_ex("1,2t0"),
            Ex::Copy(SubRange::Lines(1, 2), LineAddr::Line(0))
        );
        assert_eq!(
            parse_ex("copy."),
            Ex::Copy(SubRange::CurrentLine, LineAddr::Current)
        );
        assert_eq!(
            parse_ex("t5"),
            Ex::Copy(SubRange::CurrentLine, LineAddr::Line(5))
        );
        // A move/copy with no destination is not a valid command → Unknown.
        assert!(matches!(parse_ex("m"), Ex::Unknown(_)));
    }

    #[test]
    fn parse_put() {
        use ruse_core::LineAddr;
        // Bare `:put` / `:pu` → the unnamed register after the current line.
        assert_eq!(
            parse_ex("put"),
            Ex::Put {
                addr: LineAddr::Current,
                reg: None
            }
        );
        assert_eq!(
            parse_ex("pu"),
            Ex::Put {
                addr: LineAddr::Current,
                reg: None
            }
        );
        // Addressed puts: `:2put` (after line 2), `:0put` (top), `:$put` (last line), `:.put` (current).
        assert_eq!(
            parse_ex("2put"),
            Ex::Put {
                addr: LineAddr::Line(2),
                reg: None
            }
        );
        assert_eq!(
            parse_ex("0put"),
            Ex::Put {
                addr: LineAddr::Line(0),
                reg: None
            }
        );
        assert_eq!(
            parse_ex("$put"),
            Ex::Put {
                addr: LineAddr::Last,
                reg: None
            }
        );
        assert_eq!(
            parse_ex(".put"),
            Ex::Put {
                addr: LineAddr::Current,
                reg: None
            }
        );
        // A register argument (whitespace-separated): named, numbered, and the clipboard register.
        assert_eq!(
            parse_ex("put a"),
            Ex::Put {
                addr: LineAddr::Current,
                reg: Some('a')
            }
        );
        assert_eq!(
            parse_ex("2put a"),
            Ex::Put {
                addr: LineAddr::Line(2),
                reg: Some('a')
            }
        );
        assert_eq!(
            parse_ex("put +"),
            Ex::Put {
                addr: LineAddr::Current,
                reg: Some('+')
            }
        );
        assert_eq!(
            parse_ex("pu 0"),
            Ex::Put {
                addr: LineAddr::Current,
                reg: Some('0')
            }
        );
        // The register must be separated by whitespace — `:puta` is not `:put a` (Vim E492) → Unknown.
        assert!(matches!(parse_ex("puta"), Ex::Unknown(_)));
        // A two-char register argument is not the single-char form → Unknown.
        assert!(matches!(parse_ex("put ab"), Ex::Unknown(_)));
    }

    #[test]
    fn parse_edit_reload() {
        assert_eq!(parse_ex("e!"), Ex::EditReload);
        assert_eq!(parse_ex("edit!"), Ex::EditReload);
    }

    #[test]
    fn parse_time_travel() {
        assert_eq!(parse_ex("earlier"), Ex::Earlier(1));
        assert_eq!(parse_ex("ea 3"), Ex::Earlier(3));
        assert_eq!(parse_ex("later"), Ex::Later(1));
        assert_eq!(parse_ex("lat 5"), Ex::Later(5));
        // A non-numeric count is not understood → Unknown.
        assert!(matches!(parse_ex("earlier x"), Ex::Unknown(_)));
    }

    #[test]
    fn parse_set_options() {
        use ruse_core::EditorOption;
        assert_eq!(parse_ex("set ic"), Ex::Set(EditorOption::IgnoreCase(true)));
        assert_eq!(
            parse_ex("set noignorecase"),
            Ex::Set(EditorOption::IgnoreCase(false))
        );
        assert_eq!(parse_ex("set scs"), Ex::Set(EditorOption::SmartCase(true)));
        assert_eq!(
            parse_ex("set expandtab"),
            Ex::Set(EditorOption::ExpandTab(true))
        );
        assert_eq!(
            parse_ex("set noet"),
            Ex::Set(EditorOption::ExpandTab(false))
        );
        assert_eq!(parse_ex("set sw=2"), Ex::Set(EditorOption::ShiftWidth(2)));
        assert_eq!(
            parse_ex("set shiftwidth=8"),
            Ex::Set(EditorOption::ShiftWidth(8))
        );
        // Unknown option → not a Set (falls through to Unknown).
        assert!(matches!(parse_ex("set bogus"), Ex::Unknown(_)));
        assert!(matches!(parse_ex("set"), Ex::Unknown(_)));
    }

    #[test]
    fn parse_buffer_delete() {
        assert_eq!(parse_ex("bd"), Ex::BufferDelete { force: false });
        assert_eq!(parse_ex("bdelete"), Ex::BufferDelete { force: false });
        assert_eq!(parse_ex("bd!"), Ex::BufferDelete { force: true });
        assert_eq!(parse_ex("bdelete!"), Ex::BufferDelete { force: true });
    }

    #[test]
    fn parse_sort_flags() {
        use ruse_core::SubRange;
        // No range → whole file; flags parse; `!` = reverse.
        match parse_ex("sort! n") {
            Ex::Sort(r, s) => {
                assert_eq!(r, SubRange::WholeFile);
                assert!(s.reverse && s.numeric && !s.unique);
            }
            other => panic!("expected Sort, got {other:?}"),
        }
        match parse_ex("1,3sort u") {
            Ex::Sort(r, s) => {
                assert_eq!(r, SubRange::Lines(1, 3));
                assert!(!s.reverse && !s.numeric && s.unique);
            }
            other => panic!("expected Sort, got {other:?}"),
        }
        // `i` = case-insensitive.
        match parse_ex("sort i") {
            Ex::Sort(_, s) => assert!(s.ignore_case && s.pattern.is_none()),
            other => panic!("expected Sort, got {other:?}"),
        }
        // `/pattern/` — the pattern is captured (delimiters stripped); `r` alone (no pattern) stays inert.
        match parse_ex("sort /id=/") {
            Ex::Sort(_, s) => {
                assert_eq!(s.pattern.as_deref(), Some("id="));
                assert!(!s.use_match);
            }
            other => panic!("expected Sort, got {other:?}"),
        }
        // `/pattern/ r` — sort on the matched text (order-independent: flags precede the pattern in Vim, so
        // this is written `sort r /pat/`).
        match parse_ex("sort rn /\\d\\+/") {
            Ex::Sort(_, s) => {
                assert_eq!(s.pattern.as_deref(), Some("\\d\\+"));
                assert!(s.use_match && s.numeric);
            }
            other => panic!("expected Sort, got {other:?}"),
        }
    }

    #[test]
    fn doubled_operator_is_linewise() {
        assert_eq!(feed("dd"), Feed::Cmd(Command::Delete(1, Motion::Line)));
        assert_eq!(feed("2dd"), Feed::Cmd(Command::Delete(2, Motion::Line)));
        assert_eq!(feed("cc"), Feed::Cmd(Command::Change(1, Motion::Line)));
    }

    #[test]
    fn forced_wise_after_operator() {
        // `v`/`V` after an operator FORCE the next motion's wise (Vim o_v/o_V) → OpForced.
        assert_eq!(
            feed("dvj"),
            Feed::Cmd(Command::OpForced {
                op: OpKind::Delete,
                count: 1,
                motion: Motion::Down,
                wise: ForcedWise::Charwise,
            })
        );
        assert_eq!(
            feed("dVe"),
            Feed::Cmd(Command::OpForced {
                op: OpKind::Delete,
                count: 1,
                motion: Motion::WordEnd,
                wise: ForcedWise::Linewise,
            })
        );
        // Count still multiplies through the forced form (`y2Vj`).
        assert_eq!(
            feed("y2Vj"),
            Feed::Cmd(Command::OpForced {
                op: OpKind::Yank,
                count: 2,
                motion: Motion::Down,
                wise: ForcedWise::Linewise,
            })
        );
    }

    #[test]
    fn bare_v_still_enters_visual() {
        // Without an operator armed, `v`/`V` enter Visual as before — the force only applies operator-pending.
        assert_eq!(
            feed("v"),
            Feed::Cmd(Command::EnterVisual {
                kind: SelectKind::Charwise
            })
        );
        assert_eq!(
            feed("V"),
            Feed::Cmd(Command::EnterVisual {
                kind: SelectKind::Linewise
            })
        );
    }

    #[test]
    fn cw_passes_wordfwd_for_the_core_special_case() {
        // `cw` now emits Change(WordFwd); the core's change_range applies Vim's cw-word-end rule (which
        // differs from `ce`/WordEnd at a word's last char). Rewriting to WordEnd here was the old bug.
        assert_eq!(feed("cw"), Feed::Cmd(Command::Change(1, Motion::WordFwd)));
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn bracket_paste_emits_indent_adjusting_paste() {
        // `]p` pastes AFTER; `]P`, `[p`, `[P` all paste BEFORE (Vim). A count carries through (`2]p`).
        assert_eq!(
            feed("]p"),
            Feed::Cmd(Command::PasteIndent {
                after: true,
                count: 1
            })
        );
        assert_eq!(
            feed("]P"),
            Feed::Cmd(Command::PasteIndent {
                after: false,
                count: 1
            })
        );
        assert_eq!(
            feed("[p"),
            Feed::Cmd(Command::PasteIndent {
                after: false,
                count: 1
            })
        );
        assert_eq!(
            feed("[P"),
            Feed::Cmd(Command::PasteIndent {
                after: false,
                count: 1
            })
        );
        assert_eq!(
            feed("2]p"),
            Feed::Cmd(Command::PasteIndent {
                after: true,
                count: 2
            })
        );
        // An unwired bracket command aborts cleanly.
        assert_eq!(feed("]x"), Feed::Ignored);
    }

    #[test]
    fn insert_ctrl_w_and_ctrl_u_emit_delete_commands() {
        let mut e = InputEngine::new();
        assert_eq!(
            e.feed(ctrl('w'), Mode::Insert),
            Feed::Cmd(Command::InsertDeleteWordBack)
        );
        assert_eq!(
            e.feed(ctrl('u'), Mode::Insert),
            Feed::Cmd(Command::InsertDeleteToLineStart)
        );
    }

    #[test]
    fn g_question_is_the_rot13_operator() {
        use ruse_core::WordCase;
        // `g?$` → ROT13 to end of line.
        assert_eq!(
            feed("g?$"),
            Feed::Cmd(Command::CaseMotion {
                count: 1,
                motion: Motion::LineEnd,
                case: WordCase::Rot13,
            })
        );
        // `g?ap` → ROT13 a paragraph; `g??` → the doubled linewise form.
        assert_eq!(
            feed("g?ap"),
            Feed::Cmd(Command::CaseMotion {
                count: 1,
                motion: Motion::AParagraph,
                case: WordCase::Rot13,
            })
        );
        assert_eq!(
            feed("g??"),
            Feed::Cmd(Command::CaseMotion {
                count: 1,
                motion: Motion::Line,
                case: WordCase::Rot13,
            })
        );
        // Visual `g?` recases the selection.
        let mut e = InputEngine::new();
        let vis = Mode::Visual {
            kind: SelectKind::Charwise,
        };
        assert_eq!(e.feed(k('g'), vis), Feed::Pending);
        assert_eq!(
            e.feed(k('?'), vis),
            Feed::Cmd(Command::CaseSelection(WordCase::Rot13))
        );
    }

    #[test]
    fn gq_and_gw_arm_the_reflow_operator() {
        // `gqap` → format the paragraph (cursor moves); `gwj` → format down (cursor kept).
        assert_eq!(
            feed("gqap"),
            Feed::Cmd(Command::Format {
                count: 1,
                motion: Motion::AParagraph,
                keep_cursor: false,
            })
        );
        assert_eq!(
            feed("gwj"),
            Feed::Cmd(Command::Format {
                count: 1,
                motion: Motion::Down,
                keep_cursor: true,
            })
        );
        // A count multiplies through: `2gqj`.
        assert_eq!(
            feed("2gqj"),
            Feed::Cmd(Command::Format {
                count: 2,
                motion: Motion::Down,
                keep_cursor: false,
            })
        );
    }

    #[test]
    fn set_textwidth_parses() {
        use ruse_core::EditorOption;
        assert_eq!(
            parse_ex("set textwidth=72"),
            Ex::Set(EditorOption::TextWidth(72))
        );
        assert_eq!(parse_ex("set tw=0"), Ex::Set(EditorOption::TextWidth(0)));
    }

    #[test]
    fn insert_ctrl_t_and_ctrl_d_emit_indent_commands() {
        let mut e = InputEngine::new();
        assert_eq!(
            e.feed(ctrl('t'), Mode::Insert),
            Feed::Cmd(Command::InsertIndent)
        );
        assert_eq!(
            e.feed(ctrl('d'), Mode::Insert),
            Feed::Cmd(Command::InsertDedent)
        );
    }

    #[test]
    fn insert_tab_emits_insert_tab_not_ignored() {
        // Regression: `<Tab>` in Insert used to fall through to the unmatched-Insert policy (which only
        // emits for `Char` keys) and silently do nothing. It must produce `InsertTab`.
        let mut e = InputEngine::new();
        assert_eq!(
            e.feed(
                KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
                Mode::Insert
            ),
            Feed::Cmd(Command::InsertTab)
        );
    }

    #[test]
    fn insert_ctrl_r_inserts_the_named_register() {
        let mut e = InputEngine::new();
        // `<C-r>` arms the prefix (pending), then the register name completes it.
        assert_eq!(e.feed(ctrl('r'), Mode::Insert), Feed::Pending);
        assert_eq!(
            e.feed(k('a'), Mode::Insert),
            Feed::Cmd(Command::InsertRegister('a'))
        );
        // `<C-r>"` reads the unnamed register; `<C-r>1`/`<C-r>-` reach the delete slots.
        e.feed(ctrl('r'), Mode::Insert);
        assert_eq!(
            e.feed(k('"'), Mode::Insert),
            Feed::Cmd(Command::InsertRegister('"'))
        );
        e.feed(ctrl('r'), Mode::Insert);
        assert_eq!(
            e.feed(k('1'), Mode::Insert),
            Feed::Cmd(Command::InsertRegister('1'))
        );
        // A non-register second key aborts the prefix without inserting.
        assert_eq!(e.feed(ctrl('r'), Mode::Insert), Feed::Pending);
        assert_eq!(e.feed(k(' '), Mode::Insert), Feed::Ignored);
    }

    #[test]
    fn insert_ctrl_r_equals_opens_the_expression_prompt() {
        // `<C-r>=` opens the expression-register prompt (`:help i_CTRL-R`): typing the expression is
        // Pending, and `<CR>` yields `InsertEval` carrying the collected string for the editor to evaluate.
        let mut e = InputEngine::new();
        assert_eq!(e.feed(ctrl('r'), Mode::Insert), Feed::Pending);
        assert_eq!(e.feed(k('='), Mode::Insert), Feed::Pending);
        // The prompt renders with the `=` glyph and owns the typed buffer.
        for c in "1+2".chars() {
            assert_eq!(e.feed(k(c), Mode::Insert), Feed::Pending);
        }
        assert_eq!(e.cmdline(), Some(('=', "1+2", 3)));
        assert_eq!(
            e.feed(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                Mode::Insert
            ),
            Feed::Cmd(Command::InsertEval("1+2".into()))
        );
        assert_eq!(e.cmdline(), None, "the prompt closes on <CR>");
    }

    #[test]
    fn insert_ctrl_k_inserts_a_digraph() {
        // `i_CTRL-K {c1}{c2}`: arm (Pending), first code char (Pending), second char resolves the glyph.
        let mut e = InputEngine::new();
        assert_eq!(e.feed(ctrl('k'), Mode::Insert), Feed::Pending);
        assert_eq!(e.feed(k('a'), Mode::Insert), Feed::Pending);
        assert_eq!(
            e.feed(k(':'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('ä'))
        );
        // A symbol digraph resolves the same way.
        e.feed(ctrl('k'), Mode::Insert);
        e.feed(k('O'), Mode::Insert);
        assert_eq!(
            e.feed(k('K'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('✓'))
        );
        // After a completed digraph the engine is back to plain Insert (no lingering prefix state).
        assert_eq!(
            e.feed(k('x'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('x'))
        );
    }

    #[test]
    fn insert_ctrl_k_unknown_pair_falls_back_to_second_char() {
        // Vim's fallback for an unknown code: insert the SECOND char literally.
        let mut e = InputEngine::new();
        e.feed(ctrl('k'), Mode::Insert);
        e.feed(k('z'), Mode::Insert);
        assert_eq!(
            e.feed(k('q'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('q'))
        );
    }

    #[test]
    fn insert_ctrl_k_esc_mid_sequence_aborts_cleanly() {
        // A non-printable key mid-digraph cancels the pending state without inserting; Insert then resumes.
        let mut e = InputEngine::new();
        e.feed(ctrl('k'), Mode::Insert);
        e.feed(k('a'), Mode::Insert); // first code char collected
        assert_eq!(
            e.feed(
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                Mode::Insert
            ),
            Feed::Ignored
        );
        // The abort left no residue: the next key inserts literally.
        assert_eq!(
            e.feed(k('b'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('b'))
        );
    }

    #[test]
    fn insert_ctrl_k_code_chars_are_not_lang_mapped() {
        // The two digraph-code chars are literal selectors, so an active Lang-Arg map must NOT rewrite
        // them (else `a:` under a map `a->б` would look up the wrong code).
        let mut e = InputEngine::new();
        e.lang_map.insert('a', 'б');
        e.lang_active = true;
        e.feed(ctrl('k'), Mode::Insert);
        e.feed(k('a'), Mode::Insert);
        assert_eq!(
            e.feed(k(':'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('ä'))
        );
    }

    // --- `i_CTRL-V` literal / numeric char entry (all expectations verified against nvim v0.12.4). ---

    #[test]
    fn insert_ctrl_v_decimal_octal_hex_and_unicode() {
        let mut e = InputEngine::new();
        // `C-v 065` -> decimal 65 -> `A` (resolves on the 3rd digit; each earlier key is Pending).
        assert_eq!(e.feed(ctrl('v'), Mode::Insert), Feed::Pending);
        assert_eq!(e.feed(k('0'), Mode::Insert), Feed::Pending);
        assert_eq!(e.feed(k('6'), Mode::Insert), Feed::Pending);
        assert_eq!(
            e.feed(k('5'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('A'))
        );
        // `C-v x41` -> hex byte 0x41 -> `A` (resolves on the 2nd hex digit).
        e.feed(ctrl('v'), Mode::Insert);
        e.feed(k('x'), Mode::Insert);
        assert_eq!(e.feed(k('4'), Mode::Insert), Feed::Pending);
        assert_eq!(
            e.feed(k('1'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('A'))
        );
        // `C-v o101` -> octal 101 (= 65) -> `A`.
        e.feed(ctrl('v'), Mode::Insert);
        e.feed(k('o'), Mode::Insert);
        e.feed(k('1'), Mode::Insert);
        e.feed(k('0'), Mode::Insert);
        assert_eq!(
            e.feed(k('1'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('A'))
        );
        // `C-v u00e9` -> BMP U+00E9 -> `é` (resolves on the 4th hex digit).
        e.feed(ctrl('v'), Mode::Insert);
        e.feed(k('u'), Mode::Insert);
        e.feed(k('0'), Mode::Insert);
        e.feed(k('0'), Mode::Insert);
        e.feed(k('e'), Mode::Insert);
        assert_eq!(
            e.feed(k('9'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('é'))
        );
        // `C-v U0001F600` -> full Unicode U+1F600 -> 😀 (resolves on the 8th hex digit).
        e.feed(ctrl('v'), Mode::Insert);
        for c in "U0001F60".chars() {
            assert_eq!(e.feed(k(c), Mode::Insert), Feed::Pending);
        }
        assert_eq!(
            e.feed(k('0'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('😀'))
        );
    }

    #[test]
    fn insert_ctrl_v_decimal_clamps_to_a_single_byte() {
        // nvim caps a decimal (and octal) `C-v` code at 255: `C-v 999` -> `ÿ` (U+00FF), `C-v 200` -> `È`.
        let mut e = InputEngine::new();
        e.feed(ctrl('v'), Mode::Insert);
        e.feed(k('9'), Mode::Insert);
        e.feed(k('9'), Mode::Insert);
        assert_eq!(
            e.feed(k('9'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('\u{ff}'))
        );
        e.feed(ctrl('v'), Mode::Insert);
        e.feed(k('2'), Mode::Insert);
        e.feed(k('0'), Mode::Insert);
        assert_eq!(
            e.feed(k('0'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('È'))
        );
    }

    #[test]
    fn insert_ctrl_v_early_terminator_resolves_then_reprocesses_the_key() {
        // `C-v 9x`: `9` alone is a valid decimal digit (Pending); `x` is not, so the code resolves to
        // char 9 (a tab) AND `x` is then inserted as normal input. Both reach the driver via `Replay`.
        let mut e = InputEngine::new();
        assert_eq!(e.feed(ctrl('v'), Mode::Insert), Feed::Pending);
        assert_eq!(e.feed(k('9'), Mode::Insert), Feed::Pending);
        assert_eq!(
            e.feed(k('x'), Mode::Insert),
            Feed::Replay(vec![Command::InsertChar('\t'), Command::InsertChar('x'),])
        );
        // A two-digit decimal then a letter terminator: `C-v 65a` -> `A` then `a`.
        e.feed(ctrl('v'), Mode::Insert);
        e.feed(k('6'), Mode::Insert);
        e.feed(k('5'), Mode::Insert);
        assert_eq!(
            e.feed(k('a'), Mode::Insert),
            Feed::Replay(vec![Command::InsertChar('A'), Command::InsertChar('a'),])
        );
    }

    #[test]
    fn insert_ctrl_v_terminator_that_leaves_insert() {
        // `C-v 65<Esc>` inserts `A` then the `<Esc>` leaves Insert (nvim behaviour), so the resolved char
        // and `EnterNormal` both reach the driver.
        let mut e = InputEngine::new();
        e.feed(ctrl('v'), Mode::Insert);
        e.feed(k('6'), Mode::Insert);
        e.feed(k('5'), Mode::Insert);
        assert_eq!(
            e.feed(esc(), Mode::Insert),
            Feed::Replay(vec![Command::InsertChar('A'), Command::EnterNormal])
        );
    }

    #[test]
    fn insert_ctrl_v_max_digits_then_next_key_is_normal_input() {
        // After the digit cap is reached the collector is done: the following key is ordinary Insert input.
        let mut e = InputEngine::new();
        e.feed(ctrl('v'), Mode::Insert);
        e.feed(k('0'), Mode::Insert);
        e.feed(k('6'), Mode::Insert);
        assert_eq!(
            e.feed(k('5'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('A'))
        );
        assert_eq!(
            e.feed(k('Z'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('Z'))
        );
    }

    #[test]
    fn count_on_insert_esc_replays_extra_repeats_then_leaves_insert() {
        // `3ihello<Esc>` (VIM-CNT-INS): entry + first "hello" flow through as `Cmd`s; the terminating
        // `<Esc>` returns the (count-1) EXTRA "hello"s plus `EnterNormal` as ONE `Feed::Replay`, so the
        // whole run collapses into a single undo group at the driver.
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('3'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.feed(k('i'), Mode::Normal),
            Feed::Cmd(Command::EnterInsert)
        );
        for c in "hello".chars() {
            assert_eq!(
                e.feed(k(c), Mode::Insert),
                Feed::Cmd(Command::InsertChar(c))
            );
        }
        let mut tail: Vec<Command> = Vec::new();
        for _ in 0..2 {
            tail.extend("hello".chars().map(Command::InsertChar));
        }
        tail.push(Command::EnterNormal);
        assert_eq!(e.feed(esc(), Mode::Insert), Feed::Replay(tail));
    }

    #[test]
    fn count_on_insert_open_below_replay_reopens_a_line_each_repeat() {
        // `2ox<Esc>`: the extra repeat must OPEN a new line (not append to the same one), so the replay
        // tail is `[OpenBelow, InsertChar('x'), EnterNormal]`.
        let mut e = InputEngine::new();
        e.feed(k('2'), Mode::Normal);
        assert_eq!(e.feed(k('o'), Mode::Normal), Feed::Cmd(Command::OpenBelow));
        assert_eq!(
            e.feed(k('x'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('x'))
        );
        assert_eq!(
            e.feed(esc(), Mode::Insert),
            Feed::Replay(vec![
                Command::OpenBelow,
                Command::InsertChar('x'),
                Command::EnterNormal,
            ])
        );
    }

    #[test]
    fn count_one_insert_esc_is_a_plain_enter_normal() {
        // Regression: a count-less insert must be UNCHANGED — the `<Esc>` is still a bare `Cmd(EnterNormal)`,
        // never a `Feed::Replay`.
        let mut e = InputEngine::new();
        assert_eq!(
            e.feed(k('i'), Mode::Normal),
            Feed::Cmd(Command::EnterInsert)
        );
        assert_eq!(
            e.feed(k('x'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('x'))
        );
        assert_eq!(e.feed(esc(), Mode::Insert), Feed::Cmd(Command::EnterNormal));
    }

    #[test]
    fn insert_ctrl_v_prefix_with_no_digits_inserts_the_terminator_literally() {
        // `C-v x z`: the `x` hex prefix collects zero digits, so nvim inserts the terminator `z` and no
        // code char. Likewise `C-v o 8` -> `8` (8 is not an octal digit).
        let mut e = InputEngine::new();
        e.feed(ctrl('v'), Mode::Insert);
        assert_eq!(e.feed(k('x'), Mode::Insert), Feed::Pending);
        assert_eq!(
            e.feed(k('z'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('z'))
        );
        e.feed(ctrl('v'), Mode::Insert);
        e.feed(k('o'), Mode::Insert);
        assert_eq!(
            e.feed(k('8'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('8'))
        );
    }

    #[test]
    fn insert_ctrl_v_inserts_special_keys_literally() {
        // `C-v` then a non-form key inserts it verbatim: a real tab, a `<CR>`, the escape char, or the
        // control byte of another `C-<key>` (`C-v C-v` -> 0x16). All verified against nvim.
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let mut e = InputEngine::new();
        assert_eq!(e.feed(ctrl('v'), Mode::Insert), Feed::Pending);
        assert_eq!(
            e.feed(tab, Mode::Insert),
            Feed::Cmd(Command::InsertChar('\t'))
        );
        e.feed(ctrl('v'), Mode::Insert);
        assert_eq!(
            e.feed(enter, Mode::Insert),
            Feed::Cmd(Command::InsertChar('\r'))
        );
        e.feed(ctrl('v'), Mode::Insert);
        assert_eq!(
            e.feed(esc(), Mode::Insert),
            Feed::Cmd(Command::InsertChar('\u{1b}'))
        );
        e.feed(ctrl('v'), Mode::Insert);
        assert_eq!(
            e.feed(ctrl('v'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('\u{16}'))
        );
        // A plain non-form char is also literal: `C-v z` -> `z`.
        e.feed(ctrl('v'), Mode::Insert);
        assert_eq!(
            e.feed(k('z'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('z'))
        );
    }

    #[test]
    fn insert_ctrl_v_code_keys_are_not_lang_mapped() {
        // The `C-v` form selector and code digits are raw selectors, so an active Lang-Arg map must NOT
        // rewrite them. With `0 -> 9` mapped, `C-v 065` still resolves the raw `065` (= `A`), not `965`.
        let mut e = InputEngine::new();
        e.lang_map.insert('0', '9');
        e.lang_active = true;
        e.feed(ctrl('v'), Mode::Insert);
        e.feed(k('0'), Mode::Insert);
        e.feed(k('6'), Mode::Insert);
        assert_eq!(
            e.feed(k('5'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('A'))
        );
    }

    #[test]
    fn normal_quote_equals_opens_the_expression_prompt_for_paste() {
        // `"=` opens the expression prompt; `<CR>` yields `SetExprRegister`, which arms the `"=` register so
        // the FOLLOWING p/P pastes the result (`:help quote=`).
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('"'), Mode::Normal), Feed::Pending);
        assert_eq!(e.feed(k('='), Mode::Normal), Feed::Pending);
        for c in "'a'.'b'".chars() {
            assert_eq!(e.feed(k(c), Mode::Normal), Feed::Pending);
        }
        assert_eq!(e.cmdline(), Some(('=', "'a'.'b'", 7)));
        assert_eq!(
            e.feed(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                Mode::Normal
            ),
            Feed::Cmd(Command::SetExprRegister("'a'.'b'".into()))
        );
    }

    #[test]
    fn expression_prompt_aborts_on_escape() {
        // `<Esc>` during the expression prompt closes it without emitting a command (Vim's cancel).
        let mut e = InputEngine::new();
        e.feed(k('"'), Mode::Normal);
        e.feed(k('='), Mode::Normal);
        e.feed(k('1'), Mode::Normal);
        assert_eq!(
            e.feed(
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                Mode::Normal
            ),
            Feed::Ignored
        );
        assert_eq!(e.cmdline(), None, "the prompt is gone after <Esc>");
    }

    #[test]
    fn ctrl_o_ctrl_i_and_tab_walk_the_jumplist() {
        let mut e = InputEngine::new();
        assert_eq!(
            e.feed(ctrl('o'), Mode::Normal),
            Feed::Cmd(Command::GotoOlderJump)
        );
        assert_eq!(
            e.feed(ctrl('i'), Mode::Normal),
            Feed::Cmd(Command::GotoNewerJump)
        );
        // CTRL-I is Tab in a terminal, so plain Tab is also forward.
        assert_eq!(
            e.feed(
                KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
                Mode::Normal
            ),
            Feed::Cmd(Command::GotoNewerJump)
        );
    }

    #[test]
    fn ctrl_a_and_ctrl_x_increment_decrement_by_count() {
        let mut e = InputEngine::new();
        assert_eq!(
            e.feed(ctrl('a'), Mode::Normal),
            Feed::Cmd(Command::IncrementNumber(1))
        );
        assert_eq!(
            e.feed(ctrl('x'), Mode::Normal),
            Feed::Cmd(Command::IncrementNumber(-1))
        );
        // `3<C-a>` adds 3; `2<C-x>` subtracts 2.
        assert_eq!(e.feed(k('3'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.feed(ctrl('a'), Mode::Normal),
            Feed::Cmd(Command::IncrementNumber(3))
        );
        assert_eq!(e.feed(k('2'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.feed(ctrl('x'), Mode::Normal),
            Feed::Cmd(Command::IncrementNumber(-2))
        );
    }

    #[test]
    fn visual_ctrl_a_ctrl_x_increment_the_selection() {
        let vis = Mode::Visual {
            kind: SelectKind::Linewise,
        };
        // Plain Visual `CTRL-A`/`CTRL-X` — bump every selected line by ±count.
        let mut e = InputEngine::new();
        assert_eq!(
            e.feed(ctrl('a'), vis),
            Feed::Cmd(Command::IncrementSelection {
                delta: 1,
                sequential: false,
            })
        );
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('3'), vis), Feed::Pending);
        assert_eq!(
            e.feed(ctrl('x'), vis),
            Feed::Cmd(Command::IncrementSelection {
                delta: -3,
                sequential: false,
            })
        );
        // `g CTRL-A`/`g CTRL-X` — the sequential form (turn a column into a run).
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('g'), vis), Feed::Pending);
        assert_eq!(
            e.feed(ctrl('a'), vis),
            Feed::Cmd(Command::IncrementSelection {
                delta: 1,
                sequential: true,
            })
        );
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('g'), vis), Feed::Pending);
        assert_eq!(
            e.feed(ctrl('x'), vis),
            Feed::Cmd(Command::IncrementSelection {
                delta: -1,
                sequential: true,
            })
        );
    }

    #[test]
    fn emacs_profile_is_non_modal() {
        // F-012 seam: the Emacs profile resolves global-map C- motions to commands and self-inserts a
        // printable key — regardless of the (Vim) mode passed, and without the modal grammar.
        let mut e = InputEngine::emacs();
        assert_eq!(
            e.feed(ctrl('f'), Mode::Insert),
            Feed::Cmd(Command::MoveRight)
        );
        assert_eq!(
            e.feed(ctrl('b'), Mode::Insert),
            Feed::Cmd(Command::MoveLeft)
        );
        assert_eq!(
            e.feed(ctrl('n'), Mode::Insert),
            Feed::Cmd(Command::MoveDown)
        );
        assert_eq!(e.feed(ctrl('p'), Mode::Insert), Feed::Cmd(Command::MoveUp));
        assert_eq!(
            e.feed(ctrl('a'), Mode::Insert),
            Feed::Cmd(Command::MoveLineStart)
        );
        assert_eq!(
            e.feed(ctrl('e'), Mode::Insert),
            Feed::Cmd(Command::MoveLineEnd)
        );
        // A printable key self-inserts (no Insert mode needed) — proof of "move + insert in one state".
        assert_eq!(
            e.feed(k('x'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('x'))
        );
        // `d` is a literal char here, NOT the Vim delete operator (the modal grammar is not consulted).
        assert_eq!(
            e.feed(k('d'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('d'))
        );
        // An unbound control key is inert (not the start of a Vim construct).
        assert_eq!(e.feed(ctrl('z'), Mode::Insert), Feed::Ignored);
    }

    /// Feed `seq` through a NATIVE-profile engine (F-013), mirroring the Vim `feed` helper.
    fn feed_native(seq: &str) -> Feed {
        let mut e = InputEngine::native();
        let mut last = Feed::Ignored;
        for c in seq.chars() {
            last = e.feed(k(c), Mode::Normal);
        }
        last
    }

    #[test]
    fn native_profile_text_layer_is_the_vim_modal_grammar() {
        // F-013 NAT-1: the Native profile's TEXT layer REUSES the Vim operator/motion/text-object grammar.
        // In this slice that is the whole observable behaviour — Native drives text identically to Vim, so
        // every modal construct resolves to the same Command. (The additive leader/which-key and transient
        // layers land in later slices; they do not alter the text grammar asserted here.)
        for seq in [
            "w", "3w", "dw", "d2w", "2dw", "2d3w", "dd", "2dd", "dG", "dgg", "i", "x",
        ] {
            assert_eq!(
                feed_native(seq),
                feed(seq),
                "Native must resolve {seq:?} identically to Vim (NAT-1)"
            );
        }
    }

    #[test]
    fn native_profile_is_modal_not_non_modal() {
        // The Native profile takes the MODAL path, NOT the Emacs non-modal one: an operator+motion composes
        // (`dw` = a delete over a word), and `C-f` is inert in Normal (a Vim construct start / unbound), NOT
        // the Emacs `MoveRight` self-resolving command. This pins Native to the Vim branch of `feed`.
        assert_eq!(
            feed_native("dw"),
            Feed::Cmd(Command::Delete(1, Motion::WordFwd))
        );
        let mut e = InputEngine::native();
        assert_ne!(
            e.feed(ctrl('f'), Mode::Normal),
            Feed::Cmd(Command::MoveRight)
        );
    }

    #[test]
    fn native_leader_opens_whichkey_and_resolves_to_semantic_commands() {
        // F-013 NAT-2: `<leader>` (Space) from a clean Normal base ARMS the which-key tier (Pending, no
        // command yet); the next key resolves in the leader map to a SEMANTIC command (INV-CMD-SEMANTIC).
        let mut e = InputEngine::native();
        assert_eq!(e.feed(k(' '), Mode::Normal), Feed::Pending);
        assert!(e.leader_hint().is_some(), "Space must arm the leader tier");
        assert_eq!(e.feed(k('w'), Mode::Normal), Feed::Cmd(Command::Save));
        assert!(e.leader_hint().is_none(), "the selection disarms the tier");

        // Each seed binding resolves to its semantic command.
        for (key_char, cmd) in [
            ('w', Command::Save),
            ('q', Command::Quit),
            ('u', Command::Undo),
            ('r', Command::Redo),
        ] {
            let mut e = InputEngine::native();
            assert_eq!(e.feed(k(' '), Mode::Normal), Feed::Pending);
            assert_eq!(e.feed(k(key_char), Mode::Normal), Feed::Cmd(cmd));
        }

        // While armed, the discovery hint spells out the whole menu (the which-key render feeds on this).
        let e = {
            let mut e = InputEngine::native();
            e.feed(k(' '), Mode::Normal);
            e
        };
        assert_eq!(
            e.leader_hint().as_deref(),
            Some("w:write  q:quit  u:undo  r:redo")
        );
    }

    #[test]
    fn native_leader_abort_and_gating() {
        // An unbound selection key is a which-key ABORT: the tier disarms and nothing fires.
        let mut e = InputEngine::native();
        assert_eq!(e.feed(k(' '), Mode::Normal), Feed::Pending);
        assert_eq!(e.feed(k('z'), Mode::Normal), Feed::Ignored);
        assert!(e.leader_hint().is_none());

        // The leader arms ONLY from a clean base: `d<Space>` is the Vim delete-right motion (NAT-1 intact),
        // NOT a leader — and it must equal what Vim does for the same keys.
        assert_eq!(feed_native("d "), feed("d "));
        assert!(InputEngine::native().leader_hint().is_none());

        // The leader is Native-ONLY: under Vim, Space keeps its Vim meaning (unbound here) and never arms a
        // which-key tier — the profile gate, not a shared keymap, is what makes the leader Native's alone.
        let mut v = InputEngine::new();
        assert_eq!(v.feed(k(' '), Mode::Normal), feed(" "));
        assert!(v.leader_hint().is_none());
    }

    #[test]
    fn emacs_prefix_argument_multiplies_motions_and_repeats_text() {
        // F-012 / D-049: `C-u` seeds a prefix argument (default 4); digits make it explicit; a further
        // `C-u` multiplies by four. The next command consumes it OPAQUELY — a motion multiplies, a
        // self-insert repeats. Each accumulating key is Pending (consumed, not yet a command).
        let mut e = InputEngine::emacs();

        // Bare `C-u` → default 4: `C-u C-f` moves right four graphemes.
        assert_eq!(e.feed(ctrl('u'), Mode::Insert), Feed::Pending);
        assert_eq!(
            e.feed(ctrl('f'), Mode::Insert),
            Feed::Cmd(Command::Move(4, Motion::Right))
        );

        // Explicit decimal: `C-u 3 7 C-b` moves left 37.
        assert_eq!(e.feed(ctrl('u'), Mode::Insert), Feed::Pending);
        assert_eq!(e.feed(k('3'), Mode::Insert), Feed::Pending);
        assert_eq!(e.feed(k('7'), Mode::Insert), Feed::Pending);
        assert_eq!(
            e.feed(ctrl('b'), Mode::Insert),
            Feed::Cmd(Command::Move(37, Motion::Left))
        );

        // `C-u C-u` → 16: a self-insert repeats sixteen times (Replay, not a single Cmd).
        assert_eq!(e.feed(ctrl('u'), Mode::Insert), Feed::Pending);
        assert_eq!(e.feed(ctrl('u'), Mode::Insert), Feed::Pending);
        assert_eq!(
            e.feed(k('a'), Mode::Insert),
            Feed::Replay(vec![Command::InsertChar('a'); 16])
        );

        // count == 1 keeps the grapheme-aware bare motion — the no-argument path is unchanged.
        assert_eq!(
            e.feed(ctrl('f'), Mode::Insert),
            Feed::Cmd(Command::MoveRight)
        );
        // A bare digit with no pending argument is ordinary self-inserting text, not an argument.
        assert_eq!(
            e.feed(k('3'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('3'))
        );
    }

    #[test]
    fn emacs_cx_prefix_map_dispatches_second_key() {
        // F-012: `C-x` opens the extended-command prefix map — the first key is Pending, the second
        // resolves inside the map. `C-x C-s` saves, `C-x C-c` quits, `C-x u` undoes.
        let mut e = InputEngine::emacs();

        assert_eq!(e.feed(ctrl('x'), Mode::Insert), Feed::Pending);
        assert_eq!(e.feed(ctrl('s'), Mode::Insert), Feed::Cmd(Command::Save));

        assert_eq!(e.feed(ctrl('x'), Mode::Insert), Feed::Pending);
        assert_eq!(e.feed(ctrl('c'), Mode::Insert), Feed::Cmd(Command::Quit));

        assert_eq!(e.feed(ctrl('x'), Mode::Insert), Feed::Pending);
        assert_eq!(e.feed(k('u'), Mode::Insert), Feed::Cmd(Command::Undo));

        // An unbound second key (here `C-g`, keyboard-quit) cancels the prefix and is inert — and the engine
        // is not left wedged: the very next key dispatches normally in the global map.
        assert_eq!(e.feed(ctrl('x'), Mode::Insert), Feed::Pending);
        assert_eq!(e.feed(ctrl('g'), Mode::Insert), Feed::Ignored);
        assert_eq!(
            e.feed(ctrl('f'), Mode::Insert),
            Feed::Cmd(Command::MoveRight)
        );
        // A bare `C-s` with no prefix is unbound in the global map (not a save) — prefix scoping holds.
        assert_eq!(e.feed(ctrl('s'), Mode::Insert), Feed::Ignored);
    }

    #[test]
    fn emacs_kill_and_yank_over_the_unnamed_register() {
        // F-012 / D-026: `C-k` kills into the unnamed register (the depth-1 kill ring) via `EmacsKillLine`
        // (D-051) — its own command, not Vim's `Delete(1, LineEnd)`, because at EOL it kills the newline.
        // `C-y` is Emacs yank (D-051, `EmacsYank`) — paste + set the mark, distinct from Vim `P`/`Paste`;
        // it honours a prefix count.
        let mut e = InputEngine::emacs();

        assert_eq!(
            e.feed(ctrl('k'), Mode::Insert),
            Feed::Cmd(Command::EmacsKillLine)
        );
        assert_eq!(
            e.feed(ctrl('y'), Mode::Insert),
            Feed::Cmd(Command::EmacsYank { count: 1 })
        );
        // `C-u 3 C-y` yanks three copies — the prefix argument is the yank's repeat count.
        assert_eq!(e.feed(ctrl('u'), Mode::Insert), Feed::Pending);
        assert_eq!(e.feed(k('3'), Mode::Insert), Feed::Pending);
        assert_eq!(
            e.feed(ctrl('y'), Mode::Insert),
            Feed::Cmd(Command::EmacsYank { count: 3 })
        );
    }

    #[test]
    fn emacs_shift_tracked_only_for_non_char_keys() {
        let mut e = InputEngine::emacs();
        // Shift IS meaningful on a non-char key: C-S-<backspace> is kill-whole-line.
        let cs_bs = KeyEvent::new(
            KeyCode::Backspace,
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert_eq!(
            e.feed(cs_bs, Mode::Insert),
            Feed::Cmd(Command::EmacsKillWholeLine)
        );
        // A printable key that arrives WITH a Shift modifier still matches its unshifted binding — Shift is
        // folded into the char, so M-@ (incidental Shift) is still mark-word, not a lookup miss.
        let m_at_shift = KeyEvent::new(KeyCode::Char('@'), KeyModifiers::ALT | KeyModifiers::SHIFT);
        assert_eq!(
            e.feed(m_at_shift, Mode::Insert),
            Feed::Cmd(Command::EmacsMarkWord)
        );
        // Plain M-DEL (no shift) stays backward-kill-word — the new C-S-<backspace> does not shadow it.
        let m_del = KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT);
        assert_eq!(
            e.feed(m_del, Mode::Insert),
            Feed::Cmd(Command::EmacsBackwardKillWord { count: 1 })
        );
    }

    #[test]
    fn emacs_meta_tier_word_motions_and_buffer_ends() {
        // F-012: the Meta (`M-`, Alt) tier — word motions and buffer ends. Word motions honour the prefix
        // count; `M-<`/`M->` (buffer ends) ignore it. A plain `f` still self-inserts (no Alt, no C-).
        let mut e = InputEngine::emacs();

        // `M-f` (Emacs forward-word) rests point AFTER the word (between-char), so it uses EmacsWordFwd,
        // not Vim `w`/WordFwd (which jumps to the next word start).
        assert_eq!(
            e.feed(meta('f'), Mode::Insert),
            Feed::Cmd(Command::Move(1, Motion::EmacsWordFwd))
        );
        // `M-b` (Emacs backward-word) is the two-class mirror of `M-f` — it uses EmacsWordBack, not Vim `b`/
        // WordBack (which treats a punctuation run as its own word and `_` as a word char).
        assert_eq!(
            e.feed(meta('b'), Mode::Insert),
            Feed::Cmd(Command::Move(1, Motion::EmacsWordBack))
        );
        // `M-d` kills a word forward into the register (kill ring) — its own `EmacsKillWord` command (D-051),
        // distinct from Vim `Delete` so consecutive Emacs kills accumulate and Vim deletes never do.
        assert_eq!(
            e.feed(meta('d'), Mode::Insert),
            Feed::Cmd(Command::EmacsKillWord { count: 1 })
        );
        // Buffer ends: `M-<` to the absolute buffer start, `M->` to the absolute end — count-agnostic, and
        // each pushes the mark (D-051, `EmacsBufferEdge`), distinct from Vim `gg`/`G`.
        assert_eq!(
            e.feed(meta('<'), Mode::Insert),
            Feed::Cmd(Command::EmacsBufferEdge { start: true })
        );
        assert_eq!(
            e.feed(meta('>'), Mode::Insert),
            Feed::Cmd(Command::EmacsBufferEdge { start: false })
        );

        // The prefix argument multiplies a word motion: `C-u M-f` = forward four words.
        assert_eq!(e.feed(ctrl('u'), Mode::Insert), Feed::Pending);
        assert_eq!(
            e.feed(meta('f'), Mode::Insert),
            Feed::Cmd(Command::Move(4, Motion::EmacsWordFwd))
        );

        // A plain printable key (no Alt) still self-inserts — the Meta tier does not shadow text entry.
        assert_eq!(
            e.feed(k('f'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('f'))
        );
        // An unbound Meta key is inert (not self-inserted).
        assert_eq!(e.feed(meta('z'), Mode::Insert), Feed::Ignored);
    }

    #[test]
    fn emacs_essential_editing_keys() {
        // F-012: the editing keys that make the profile usable — RET / C-j newline, DEL / C-d delete,
        // C-/ and C-_ undo. Repeatable keys honour the prefix count via Replay.
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let backspace = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        let mut e = InputEngine::emacs();

        // RET and C-j both insert a newline.
        assert_eq!(
            e.feed(enter, Mode::Insert),
            Feed::Cmd(Command::InsertNewline)
        );
        assert_eq!(
            e.feed(ctrl('j'), Mode::Insert),
            Feed::Cmd(Command::InsertNewline)
        );
        // DEL deletes the char before point; C-d is Emacs `delete-char` — deletes forward without filling
        // the kill ring (D-026: DeleteForward, not the yanking Vim `x`/DeleteUnder), with a count.
        assert_eq!(
            e.feed(backspace, Mode::Insert),
            Feed::Cmd(Command::DeleteBack)
        );
        assert_eq!(
            e.feed(ctrl('d'), Mode::Insert),
            Feed::Cmd(Command::DeleteForward(1))
        );
        // C-/ and C-_ are undo.
        assert_eq!(e.feed(ctrl('/'), Mode::Insert), Feed::Cmd(Command::Undo));
        assert_eq!(e.feed(ctrl('_'), Mode::Insert), Feed::Cmd(Command::Undo));

        // Counts: `C-u 2 RET` inserts two newlines (Replay); `C-u 3 C-d` deletes three chars (native count).
        assert_eq!(e.feed(ctrl('u'), Mode::Insert), Feed::Pending);
        assert_eq!(e.feed(k('2'), Mode::Insert), Feed::Pending);
        assert_eq!(
            e.feed(enter, Mode::Insert),
            Feed::Replay(vec![Command::InsertNewline; 2])
        );
        assert_eq!(e.feed(ctrl('u'), Mode::Insert), Feed::Pending);
        assert_eq!(e.feed(k('3'), Mode::Insert), Feed::Pending);
        assert_eq!(
            e.feed(ctrl('d'), Mode::Insert),
            Feed::Cmd(Command::DeleteForward(3))
        );
    }

    #[test]
    fn emacs_mark_and_region_bindings() {
        // F-012 / D-027: C-SPC set-mark, C-w kill-region, M-w kill-ring-save, C-x C-x exchange-point-mark.
        let ctrl_space = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL);
        let mut e = InputEngine::emacs();

        assert_eq!(
            e.feed(ctrl_space, Mode::Insert),
            Feed::Cmd(Command::SetMark)
        );
        assert_eq!(
            e.feed(ctrl('w'), Mode::Insert),
            Feed::Cmd(Command::KillRegion)
        );
        assert_eq!(
            e.feed(meta('w'), Mode::Insert),
            Feed::Cmd(Command::CopyRegion)
        );
        // C-x C-x resolves inside the C-x prefix map (first key Pending).
        assert_eq!(e.feed(ctrl('x'), Mode::Insert), Feed::Pending);
        assert_eq!(
            e.feed(ctrl('x'), Mode::Insert),
            Feed::Cmd(Command::ExchangePointMark)
        );
    }

    #[test]
    fn emacs_mx_reads_a_command_name_and_runs_it() {
        // F-012: M-x opens the minibuffer; typing a command name is Pending; <CR> resolves it via the
        // registry into a Command. An unknown name is inert.
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);

        let mut e = InputEngine::emacs();
        assert_eq!(e.feed(meta('x'), Mode::Insert), Feed::Pending);
        for c in "save-buffer".chars() {
            assert_eq!(e.feed(k(c), Mode::Insert), Feed::Pending);
        }
        assert_eq!(e.feed(enter, Mode::Insert), Feed::Cmd(Command::Save));

        // A named region command routes through M-x too.
        assert_eq!(e.feed(meta('x'), Mode::Insert), Feed::Pending);
        for c in "kill-region".chars() {
            assert_eq!(e.feed(k(c), Mode::Insert), Feed::Pending);
        }
        assert_eq!(e.feed(enter, Mode::Insert), Feed::Cmd(Command::KillRegion));

        // An unknown command name is a no-op (Emacs "[No match]").
        assert_eq!(e.feed(meta('x'), Mode::Insert), Feed::Pending);
        for c in "no-such-command".chars() {
            assert_eq!(e.feed(k(c), Mode::Insert), Feed::Pending);
        }
        assert_eq!(e.feed(enter, Mode::Insert), Feed::Ignored);
        // After the minibuffer closes, normal dispatch resumes.
        assert_eq!(
            e.feed(ctrl('f'), Mode::Insert),
            Feed::Cmd(Command::MoveRight)
        );
    }

    #[test]
    fn emacs_profile_is_a_nine_tier_stack() {
        // F-012 / D-045: the Emacs profile is ONE nine-tier LayerStack (not Vim's separate sealed
        // namespaces), walked highest-priority first down to global-map. This locks the promotion so a
        // later edit cannot silently drop a tier or reorder the walk.
        let e = InputEngine::emacs();
        assert_eq!(e.emacs.map.depth(), 9);
        let order: Vec<&str> = e.emacs.map.order().collect();
        assert_eq!(
            order.first().copied(),
            Some("emacs.keymaptier.01.overriding-terminal-local-map")
        );
        assert_eq!(
            order.last().copied(),
            Some("emacs.keymaptier.09.global-map")
        );
        // global-map carries every binding; the eight upper tiers are transparent (no minor/major modes).
        assert_eq!(
            e.emacs
                .map
                .layer("emacs.keymaptier.09.global-map")
                .map(|l| l.is_empty()),
            Some(false)
        );
        assert_eq!(
            e.emacs
                .map
                .layer("emacs.keymaptier.01.overriding-terminal-local-map")
                .map(|l| l.is_empty()),
            Some(true)
        );
    }

    #[test]
    fn ctrl_v_enters_blockwise_visual() {
        let mut e = InputEngine::new();
        assert_eq!(
            e.feed(ctrl('v'), Mode::Normal),
            Feed::Cmd(Command::EnterVisual {
                kind: SelectKind::Blockwise
            })
        );
    }

    #[test]
    fn v_slash_capital_v_slash_ctrl_v_switch_shape_or_leave() {
        let mut e = InputEngine::new();
        // From charwise: CTRL-V → blockwise, V → linewise, v → leave (Normal).
        let vis = Mode::Visual {
            kind: SelectKind::Charwise,
        };
        assert_eq!(
            e.feed(ctrl('v'), vis),
            Feed::Cmd(Command::EnterVisual {
                kind: SelectKind::Blockwise
            })
        );
        assert_eq!(e.feed(k('v'), vis), Feed::Cmd(Command::EnterNormal));
        // From blockwise: CTRL-V leaves, v → charwise, V → linewise.
        let blk = Mode::Visual {
            kind: SelectKind::Blockwise,
        };
        assert_eq!(e.feed(ctrl('v'), blk), Feed::Cmd(Command::EnterNormal));
        assert_eq!(
            e.feed(k('v'), blk),
            Feed::Cmd(Command::EnterVisual {
                kind: SelectKind::Charwise
            })
        );
        assert_eq!(
            e.feed(k('V'), blk),
            Feed::Cmd(Command::EnterVisual {
                kind: SelectKind::Linewise
            })
        );
    }

    #[test]
    fn visual_case_keys_emit_case_selection() {
        use ruse_core::WordCase;
        let mut e = InputEngine::new();
        let vis = Mode::Visual {
            kind: SelectKind::Charwise,
        };
        assert_eq!(
            e.feed(k('U'), vis),
            Feed::Cmd(Command::CaseSelection(WordCase::Upcase))
        );
        assert_eq!(
            e.feed(k('u'), vis),
            Feed::Cmd(Command::CaseSelection(WordCase::Downcase))
        );
        assert_eq!(
            e.feed(k('~'), vis),
            Feed::Cmd(Command::CaseSelection(WordCase::Toggle))
        );
        // The `g`-prefixed forms also recase in Visual: `gu`/`gU`/`g~`.
        assert_eq!(e.feed(k('g'), vis), Feed::Pending);
        assert_eq!(
            e.feed(k('u'), vis),
            Feed::Cmd(Command::CaseSelection(WordCase::Downcase))
        );
        assert_eq!(e.feed(k('g'), vis), Feed::Pending);
        assert_eq!(
            e.feed(k('U'), vis),
            Feed::Cmd(Command::CaseSelection(WordCase::Upcase))
        );
        // In SELECT mode a printable case key replaces the selection instead (namespace policy), not case
        // it — the `g`-prefix arming is Visual-only.
        let sel = Mode::Select {
            kind: SelectKind::Charwise,
        };
        assert_eq!(
            e.feed(k('U'), sel),
            Feed::Cmd(Command::ReplaceSelection('U'))
        );
    }

    #[test]
    fn visual_r_emits_replace_selection_char() {
        let mut e = InputEngine::new();
        let vis = Mode::Visual {
            kind: SelectKind::Charwise,
        };
        assert_eq!(e.feed(k('r'), vis), Feed::Pending);
        assert_eq!(
            e.feed(k('x'), vis),
            Feed::Cmd(Command::ReplaceSelectionChar('x'))
        );
        // Normal `r` is unaffected — still the count-based char replace.
        assert_eq!(feed("rz"), Feed::Cmd(Command::ReplaceChar(1, 'z')));
    }

    #[test]
    fn visual_p_and_capital_p_emit_paste_selection() {
        let mut e = InputEngine::new();
        let vis = Mode::Visual {
            kind: SelectKind::Charwise,
        };
        assert_eq!(
            e.feed(k('p'), vis),
            Feed::Cmd(Command::PasteSelection { swap: true })
        );
        assert_eq!(
            e.feed(k('P'), vis),
            Feed::Cmd(Command::PasteSelection { swap: false })
        );
    }

    #[test]
    fn block_selection_operators_route_like_any_selection() {
        let mut e = InputEngine::new();
        let blk = Mode::Visual {
            kind: SelectKind::Blockwise,
        };
        assert_eq!(e.feed(k('d'), blk), Feed::Cmd(Command::DeleteSelection));
        assert_eq!(e.feed(k('y'), blk), Feed::Cmd(Command::YankSelection));
    }

    #[test]
    fn block_mode_i_a_c_arm_insert_replicate() {
        let mut e = InputEngine::new();
        let blk = Mode::Visual {
            kind: SelectKind::Blockwise,
        };
        assert_eq!(
            e.feed(KeyEvent::new(KeyCode::Char('I'), KeyModifiers::NONE), blk),
            Feed::Cmd(Command::BlockInsert(BlockInsertKind::Insert))
        );
        assert_eq!(
            e.feed(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE), blk),
            Feed::Cmd(Command::BlockInsert(BlockInsertKind::Append))
        );
        assert_eq!(
            e.feed(k('c'), blk),
            Feed::Cmd(Command::BlockInsert(BlockInsertKind::Change))
        );
        assert_eq!(
            e.feed(k('s'), blk),
            Feed::Cmd(Command::BlockInsert(BlockInsertKind::Change))
        );
    }

    #[test]
    fn charwise_c_is_still_a_plain_change_and_lowercase_i_is_a_text_object() {
        let mut e = InputEngine::new();
        let vis = Mode::Visual {
            kind: SelectKind::Charwise,
        };
        // In charwise/linewise, `c` is the ordinary selection change (not a block-insert).
        assert_eq!(e.feed(k('c'), vis), Feed::Cmd(Command::ChangeSelection));
        // Lowercase `i` begins a text object in every shape (awaits the object key).
        let blk = Mode::Visual {
            kind: SelectKind::Blockwise,
        };
        assert_eq!(e.feed(k('i'), blk), Feed::Pending);
    }

    #[test]
    fn ctrl_v_after_operator_forces_blockwise() {
        // `d<C-v>j`: CTRL-V operator-pending forces the next motion blockwise (Vim o_CTRL-V).
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('d'), Mode::Normal), Feed::Pending);
        assert_eq!(e.feed(ctrl('v'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.feed(k('j'), Mode::Normal),
            Feed::Cmd(Command::OpForced {
                op: OpKind::Delete,
                count: 1,
                motion: Motion::Down,
                wise: ForcedWise::Blockwise,
            })
        );
    }

    #[test]
    fn ctrl_o_runs_one_normal_command_then_returns_to_insert() {
        // In Insert, CTRL-O arms a one-shot (Pending); the next key resolves through the NORMAL grammar
        // (here `x` → DeleteUnder), then the engine returns to plain Insert routing.
        let mut e = InputEngine::new();
        assert_eq!(e.feed(ctrl('o'), Mode::Insert), Feed::Pending);
        assert_eq!(
            e.feed(k('x'), Mode::Insert),
            Feed::Cmd(Command::DeleteUnder(1))
        );
        // Disarmed: the next key is an inserted char again, not a Normal command.
        assert_eq!(
            e.feed(k('x'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('x'))
        );
    }

    #[test]
    fn ctrl_o_spans_a_multi_key_normal_command() {
        // A one-shot survives the intermediate Pending keys of a multi-key command (`dw`), disarming only
        // when the command completes.
        let mut e = InputEngine::new();
        assert_eq!(e.feed(ctrl('o'), Mode::Insert), Feed::Pending);
        assert_eq!(e.feed(k('d'), Mode::Insert), Feed::Pending); // operator armed
        assert_eq!(
            e.feed(k('w'), Mode::Insert),
            Feed::Cmd(Command::Delete(1, Motion::WordFwd))
        );
        assert_eq!(
            e.feed(k('z'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('z')),
            "back to Insert after the one-shot command completes"
        );
    }

    #[test]
    fn ctrl_g_u_breaks_undo_and_other_second_keys_abort() {
        // CTRL-G is a one-key prefix in Insert: `u` (or `U`) emits BreakUndo.
        let mut e = InputEngine::new();
        assert_eq!(e.feed(ctrl('g'), Mode::Insert), Feed::Pending);
        assert_eq!(e.feed(k('u'), Mode::Insert), Feed::Cmd(Command::BreakUndo));
        // A non-`u` second key aborts the prefix without inserting; Insert then resumes normally.
        assert_eq!(e.feed(ctrl('g'), Mode::Insert), Feed::Pending);
        assert_eq!(e.feed(k('x'), Mode::Insert), Feed::Ignored);
        assert_eq!(
            e.feed(k('y'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('y'))
        );
    }

    pub(super) fn esc() -> KeyEvent {
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
    }

    fn fc(ch: char, forward: bool, till: bool) -> Motion {
        Motion::FindChar { ch, forward, till }
    }

    #[test]
    fn char_search_bare_and_operator() {
        assert_eq!(
            feed("fx"),
            Feed::Cmd(Command::Move(1, fc('x', true, false)))
        );
        assert_eq!(feed("tx"), Feed::Cmd(Command::Move(1, fc('x', true, true))));
        assert_eq!(
            feed("Fx"),
            Feed::Cmd(Command::Move(1, fc('x', false, false)))
        );
        assert_eq!(
            feed("Tx"),
            Feed::Cmd(Command::Move(1, fc('x', false, true)))
        );
        assert_eq!(
            feed("2fx"),
            Feed::Cmd(Command::Move(2, fc('x', true, false)))
        );
        // operator targets
        assert_eq!(
            feed("dtx"),
            Feed::Cmd(Command::Delete(1, fc('x', true, true)))
        );
        assert_eq!(
            feed("d2fx"),
            Feed::Cmd(Command::Delete(2, fc('x', true, false)))
        );
    }

    #[test]
    fn char_search_is_pending_until_the_target() {
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('f'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.feed(k('q'), Mode::Normal),
            Feed::Cmd(Command::Move(1, fc('q', true, false)))
        );
    }

    #[test]
    fn semicolon_repeats_comma_reverses() {
        let mut e = InputEngine::new();
        e.feed(k('f'), Mode::Normal);
        e.feed(k('x'), Mode::Normal); // last_find = (x, forward, not-till)
        assert_eq!(
            e.feed(k(';'), Mode::Normal),
            Feed::Cmd(Command::Move(1, fc('x', true, false))),
            "; repeats the last find"
        );
        assert_eq!(
            e.feed(k(','), Mode::Normal),
            Feed::Cmd(Command::Move(1, fc('x', false, false))),
            ", repeats reversed"
        );
    }

    #[test]
    fn line_jumps() {
        assert_eq!(feed("gg"), Feed::Cmd(Command::Move(1, Motion::GotoLine)));
        assert_eq!(feed("5gg"), Feed::Cmd(Command::Move(5, Motion::GotoLine)));
        assert_eq!(feed("G"), Feed::Cmd(Command::Move(1, Motion::LastLine)));
        assert_eq!(feed("5G"), Feed::Cmd(Command::Move(5, Motion::GotoLine)));
        // operator + line jump is linewise
        assert_eq!(feed("dG"), Feed::Cmd(Command::Delete(1, Motion::LastLine)));
        assert_eq!(feed("dgg"), Feed::Cmd(Command::Delete(1, Motion::GotoLine)));
    }

    #[test]
    fn single_key_edits() {
        // `r` is pending until the replacement char; ctrl-r is still redo.
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('r'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.feed(k('z'), Mode::Normal),
            Feed::Cmd(Command::ReplaceChar(1, 'z'))
        );
        assert_eq!(feed("x"), Feed::Cmd(Command::DeleteUnder(1)));
        assert_eq!(feed("~"), Feed::Cmd(Command::ToggleCase(1)));
        assert_eq!(feed("J"), Feed::Cmd(Command::JoinLines(1)));
        assert_eq!(feed("3J"), Feed::Cmd(Command::JoinLines(3)));
        assert_eq!(feed("gJ"), Feed::Cmd(Command::JoinLinesNoSpace(1)));
        assert_eq!(feed("g&"), Feed::Cmd(Command::RepeatSubstituteGlobal));
        assert_eq!(feed("`."), Feed::Cmd(Command::GotoLastChange));
        assert_eq!(feed("g;"), Feed::Cmd(Command::GotoOlderChange));
        assert_eq!(feed("g,"), Feed::Cmd(Command::GotoNewerChange));
        assert_eq!(feed("ma"), Feed::Cmd(Command::SetNamedMark('a')));
        assert_eq!(feed("`a"), Feed::Cmd(Command::GotoNamedMark('a')));
        assert_eq!(feed("gi"), Feed::Cmd(Command::InsertAtLastInsert));
        assert_eq!(feed("'a"), Feed::Cmd(Command::GotoNamedMarkLine('a')));
        assert_eq!(feed("'."), Feed::Cmd(Command::GotoLastChangeLine));
        // Counts multiply the single-key actions (Vim `3x` / `3~` / `3rz`).
        assert_eq!(feed("3x"), Feed::Cmd(Command::DeleteUnder(3)));
        assert_eq!(feed("3~"), Feed::Cmd(Command::ToggleCase(3)));
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('3'), Mode::Normal), Feed::Pending);
        assert_eq!(e.feed(k('r'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.feed(k('z'), Mode::Normal),
            Feed::Cmd(Command::ReplaceChar(3, 'z'))
        );
        let mut e = InputEngine::new();
        assert_eq!(
            e.feed(
                KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
                Mode::Normal
            ),
            Feed::Cmd(Command::Redo)
        );
    }

    #[test]
    fn shift_operators_doubled_and_counted() {
        // `>>` / `<<` are the doubled linewise forms; the count before them is the line count.
        assert_eq!(feed(">>"), Feed::Cmd(Command::ShiftRight(1)));
        assert_eq!(feed("<<"), Feed::Cmd(Command::ShiftLeft(1)));
        assert_eq!(feed("3>>"), Feed::Cmd(Command::ShiftRight(3)));
        assert_eq!(feed("2<<"), Feed::Cmd(Command::ShiftLeft(2)));
    }

    #[test]
    fn reindent_operator() {
        assert_eq!(
            feed("=="),
            Feed::Cmd(Command::Reindent {
                count: 1,
                motion: Motion::Line
            })
        );
        assert_eq!(
            feed("=G"),
            Feed::Cmd(Command::Reindent {
                count: 1,
                motion: Motion::LastLine
            })
        );
        assert_eq!(
            feed("=ap"),
            Feed::Cmd(Command::Reindent {
                count: 1,
                motion: Motion::AParagraph
            })
        );
        assert_eq!(
            feed("2=="),
            Feed::Cmd(Command::Reindent {
                count: 2,
                motion: Motion::Line
            })
        );
    }

    #[test]
    fn operator_to_mark_routing() {
        // d/c/y to a mark still route exactly as before (regression): `` ` `` charwise, `'` linewise.
        assert_eq!(
            feed("d`a"),
            Feed::Cmd(Command::OpToMark {
                op: MarkOp::Delete,
                name: 'a',
                linewise: false
            })
        );
        assert_eq!(
            feed("c'a"),
            Feed::Cmd(Command::OpToMark {
                op: MarkOp::Change,
                name: 'a',
                linewise: true
            })
        );
        assert_eq!(
            feed("y`b"),
            Feed::Cmd(Command::OpToMark {
                op: MarkOp::Yank,
                name: 'b',
                linewise: false
            })
        );
        // Case operators to a mark (`` g~`a ``/`` gu`a ``/`` gU'a ``): backtick charwise, quote linewise.
        assert_eq!(
            feed("g~`a"),
            Feed::Cmd(Command::OpToMark {
                op: MarkOp::Case(WordCase::Toggle),
                name: 'a',
                linewise: false
            })
        );
        assert_eq!(
            feed("gu`a"),
            Feed::Cmd(Command::OpToMark {
                op: MarkOp::Case(WordCase::Downcase),
                name: 'a',
                linewise: false
            })
        );
        assert_eq!(
            feed("gU'a"),
            Feed::Cmd(Command::OpToMark {
                op: MarkOp::Case(WordCase::Upcase),
                name: 'a',
                linewise: true
            })
        );
        // Shift and reindent to a mark; `linewise` records the `` ` ``/`'` key (the planner forces whole
        // lines for these regardless, per Vim).
        assert_eq!(
            feed(">`a"),
            Feed::Cmd(Command::OpToMark {
                op: MarkOp::Shift { left: false },
                name: 'a',
                linewise: false
            })
        );
        assert_eq!(
            feed("<'a"),
            Feed::Cmd(Command::OpToMark {
                op: MarkOp::Shift { left: true },
                name: 'a',
                linewise: true
            })
        );
        assert_eq!(
            feed("=`a"),
            Feed::Cmd(Command::OpToMark {
                op: MarkOp::Reindent,
                name: 'a',
                linewise: false
            })
        );
        // A bare mark jump (no operator armed) is unaffected.
        assert_eq!(feed("`c"), Feed::Cmd(Command::GotoNamedMark('c')));
        assert_eq!(feed("'c"), Feed::Cmd(Command::GotoNamedMarkLine('c')));
    }

    #[test]
    fn shift_over_motion_and_re_arm() {
        // `>`/`<` are real operators now: over a motion they shift the motion's lines (always linewise).
        assert_eq!(
            feed(">j"),
            Feed::Cmd(Command::ShiftMotion {
                left: false,
                count: 1,
                motion: Motion::Down
            })
        );
        assert_eq!(
            feed("<k"),
            Feed::Cmd(Command::ShiftMotion {
                left: true,
                count: 1,
                motion: Motion::Up
            })
        );
        assert_eq!(
            feed(">ip"),
            Feed::Cmd(Command::ShiftMotion {
                left: false,
                count: 1,
                motion: Motion::InnerParagraph
            })
        );
        assert_eq!(
            feed("2>j"),
            Feed::Cmd(Command::ShiftMotion {
                left: false,
                count: 2,
                motion: Motion::Down
            })
        );

        // Like `dc`, a second (different) operator key re-arms rather than aborting; a motion then completes it.
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('>'), Mode::Normal), Feed::Pending);
        assert_eq!(e.feed(k('<'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.feed(k('j'), Mode::Normal),
            Feed::Cmd(Command::ShiftMotion {
                left: true,
                count: 1,
                motion: Motion::Down
            })
        );
    }

    #[test]
    fn insert_entry_keys() {
        assert_eq!(feed("o"), Feed::Cmd(Command::OpenBelow));
        assert_eq!(feed("O"), Feed::Cmd(Command::OpenAbove));
        assert_eq!(feed("A"), Feed::Cmd(Command::AppendLineEnd));
        assert_eq!(feed("I"), Feed::Cmd(Command::InsertLineStart));
    }

    #[test]
    fn big_word_motions_and_cw_is_ce() {
        assert_eq!(feed("W"), Feed::Cmd(Command::Move(1, Motion::BigWordFwd)));
        assert_eq!(feed("B"), Feed::Cmd(Command::Move(1, Motion::BigWordBack)));
        assert_eq!(
            feed("dE"),
            Feed::Cmd(Command::Delete(1, Motion::BigWordEnd))
        );
        // `cw`/`cW` pass WordFwd/BigWordFwd through; the core applies the cw-word-end special case.
        assert_eq!(feed("cw"), Feed::Cmd(Command::Change(1, Motion::WordFwd)));
        assert_eq!(
            feed("cW"),
            Feed::Cmd(Command::Change(1, Motion::BigWordFwd))
        );
    }

    #[test]
    fn bracket_match() {
        assert_eq!(feed("%"), Feed::Cmd(Command::Move(1, Motion::MatchBracket)));
        assert_eq!(
            feed("d%"),
            Feed::Cmd(Command::Delete(1, Motion::MatchBracket))
        );
    }

    #[test]
    fn lone_g_is_pending_then_cancels_on_non_g() {
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('g'), Mode::Normal), Feed::Pending);
        assert_eq!(e.feed(k('z'), Mode::Normal), Feed::Ignored);
    }

    #[test]
    fn char_search_extends_visual() {
        let mut e = InputEngine::new();
        let vis = Mode::Visual {
            kind: SelectKind::Charwise,
        };
        assert_eq!(e.feed(k('f'), vis), Feed::Pending);
        assert_eq!(
            e.feed(k(')'), vis),
            Feed::Cmd(Command::Move(1, fc(')', true, false))),
            "f in Visual is a bare move that extends the selection"
        );
    }

    #[test]
    fn enters_visual_from_normal() {
        assert_eq!(
            feed("v"),
            Feed::Cmd(Command::EnterVisual {
                kind: SelectKind::Charwise
            })
        );
        assert_eq!(
            feed("V"),
            Feed::Cmd(Command::EnterVisual {
                kind: SelectKind::Linewise
            })
        );
    }

    #[test]
    fn visual_operators_act_on_the_selection() {
        let mut e = InputEngine::new();
        let vis = Mode::Visual {
            kind: SelectKind::Charwise,
        };
        assert_eq!(e.feed(k('d'), vis), Feed::Cmd(Command::DeleteSelection));
        assert_eq!(e.feed(k('y'), vis), Feed::Cmd(Command::YankSelection));
        assert_eq!(e.feed(k('c'), vis), Feed::Cmd(Command::ChangeSelection));
        assert_eq!(e.feed(k('x'), vis), Feed::Cmd(Command::DeleteSelection));
        assert_eq!(e.feed(esc(), vis), Feed::Cmd(Command::EnterNormal));
    }

    #[test]
    fn visual_o_swaps_selection_ends() {
        // In Visual/Select, `o` emits SwapSelectionEnds (in Normal it is OpenBelow).
        let mut e = InputEngine::new();
        assert_eq!(
            e.feed(
                k('o'),
                Mode::Visual {
                    kind: SelectKind::Charwise
                }
            ),
            Feed::Cmd(Command::SwapSelectionEnds)
        );
        assert_eq!(
            e.feed(
                k('o'),
                Mode::Select {
                    kind: SelectKind::Linewise
                }
            ),
            Feed::Cmd(Command::SwapSelectionEnds)
        );
        // Sanity: `o` in Normal is still OpenBelow.
        assert_eq!(feed("o"), Feed::Cmd(Command::OpenBelow));
    }

    #[test]
    fn visual_motion_extends_selection() {
        let mut e = InputEngine::new();
        let vis = Mode::Visual {
            kind: SelectKind::Charwise,
        };
        // A bare motion in Visual is a Move (no operator) — the frontend re-plans it against the anchor.
        assert_eq!(
            e.feed(k('l'), vis),
            Feed::Cmd(Command::Move(1, Motion::Right))
        );
        assert_eq!(
            e.feed(k('w'), vis),
            Feed::Cmd(Command::Move(1, Motion::WordFwd))
        );
    }

    #[test]
    fn visual_toggle_and_switch() {
        let mut e = InputEngine::new();
        // `v` in charwise Visual exits; `V` switches it to linewise.
        assert_eq!(
            e.feed(
                k('v'),
                Mode::Visual {
                    kind: SelectKind::Charwise
                }
            ),
            Feed::Cmd(Command::EnterNormal)
        );
        assert_eq!(
            e.feed(
                k('V'),
                Mode::Visual {
                    kind: SelectKind::Charwise
                }
            ),
            Feed::Cmd(Command::EnterVisual {
                kind: SelectKind::Linewise
            })
        );
        assert_eq!(
            e.feed(
                k('v'),
                Mode::Visual {
                    kind: SelectKind::Linewise
                }
            ),
            Feed::Cmd(Command::EnterVisual {
                kind: SelectKind::Charwise
            })
        );
    }

    fn ctrl_g() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL)
    }

    #[test]
    fn ctrl_g_toggles_visual_and_select_both_ways() {
        let mut e = InputEngine::new();
        // Visual -> Select, carrying the charwise/linewise shape.
        assert_eq!(
            e.feed(
                ctrl_g(),
                Mode::Visual {
                    kind: SelectKind::Charwise
                }
            ),
            Feed::Cmd(Command::EnterSelect {
                kind: SelectKind::Charwise
            })
        );
        assert_eq!(
            e.feed(
                ctrl_g(),
                Mode::Visual {
                    kind: SelectKind::Linewise
                }
            ),
            Feed::Cmd(Command::EnterSelect {
                kind: SelectKind::Linewise
            })
        );
        // Select -> Visual, back again.
        assert_eq!(
            e.feed(
                ctrl_g(),
                Mode::Select {
                    kind: SelectKind::Charwise
                }
            ),
            Feed::Cmd(Command::EnterVisual {
                kind: SelectKind::Charwise
            })
        );
        assert_eq!(
            e.feed(
                ctrl_g(),
                Mode::Select {
                    kind: SelectKind::Linewise
                }
            ),
            Feed::Cmd(Command::EnterVisual {
                kind: SelectKind::Linewise
            })
        );
        // CTRL-G is inert in Normal (no selection to toggle); it is NOT the start of `gg`.
        assert_eq!(e.feed(ctrl_g(), Mode::Normal), Feed::Ignored);
    }

    #[test]
    fn printable_key_in_select_replaces_the_selection() {
        // A key that matches no motion/operator hits Select's `open/replace-selection` policy.
        let sel = Mode::Select {
            kind: SelectKind::Charwise,
        };
        let mut e = InputEngine::new();
        assert_eq!(
            e.feed(k('z'), sel),
            Feed::Cmd(Command::ReplaceSelection('z'))
        );
        // A non-printable unmatched key does nothing.
        assert_eq!(e.feed(esc(), sel), Feed::Cmd(Command::EnterNormal));
        let mut e = InputEngine::new();
        assert_eq!(
            e.feed(k('A'), sel),
            Feed::Cmd(Command::ReplaceSelection('A'))
        );
    }

    #[test]
    fn select_operators_and_motions_match_visual() {
        let sel = Mode::Select {
            kind: SelectKind::Charwise,
        };
        let mut e = InputEngine::new();
        // d/y/c act on the selection, exactly as in Visual.
        assert_eq!(e.feed(k('d'), sel), Feed::Cmd(Command::DeleteSelection));
        assert_eq!(e.feed(k('y'), sel), Feed::Cmd(Command::YankSelection));
        assert_eq!(e.feed(k('c'), sel), Feed::Cmd(Command::ChangeSelection));
        // A motion extends the selection (a bare Move; the frontend re-plans it against the anchor).
        assert_eq!(
            e.feed(k('l'), sel),
            Feed::Cmd(Command::Move(1, Motion::Right))
        );
        assert_eq!(
            e.feed(k('w'), sel),
            Feed::Cmd(Command::Move(1, Motion::WordFwd))
        );
        // Esc leaves the selection.
        assert_eq!(e.feed(esc(), sel), Feed::Cmd(Command::EnterNormal));
    }

    #[test]
    fn named_register_prefix_parses() {
        // `"a` is pending until the name, then emits SetRegister; the following op is unaffected.
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('"'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.feed(k('a'), Mode::Normal),
            Feed::Cmd(Command::SetRegister(Some('a')))
        );
        // `"a3yy` → the count typed after the register prefix still lands.
        let mut e = InputEngine::new();
        e.feed(k('"'), Mode::Normal);
        e.feed(k('a'), Mode::Normal);
        e.feed(k('3'), Mode::Normal);
        e.feed(k('y'), Mode::Normal);
        assert_eq!(
            e.feed(k('y'), Mode::Normal),
            Feed::Cmd(Command::Yank(3, Motion::Line)),
            "count after the register prefix still applies"
        );
        // Uppercase names (append) parse too.
        let mut e = InputEngine::new();
        e.feed(k('"'), Mode::Normal);
        assert_eq!(
            e.feed(k('A'), Mode::Normal),
            Feed::Cmd(Command::SetRegister(Some('A')))
        );
        // `"` works in Visual as well (Vim `"xy`).
        let mut e = InputEngine::new();
        let vis = Mode::Visual {
            kind: SelectKind::Charwise,
        };
        assert_eq!(e.feed(k('"'), vis), Feed::Pending);
        assert_eq!(
            e.feed(k('a'), vis),
            Feed::Cmd(Command::SetRegister(Some('a')))
        );
        // The blackhole `"_` is a valid register name too.
        let mut e = InputEngine::new();
        e.feed(k('"'), Mode::Normal);
        assert_eq!(
            e.feed(k('_'), Mode::Normal),
            Feed::Cmd(Command::SetRegister(Some('_')))
        );
        // The system-clipboard registers `"+` and `"*` parse as register names (`:help quoteplus`).
        for name in ['+', '*'] {
            let mut e = InputEngine::new();
            e.feed(k('"'), Mode::Normal);
            assert_eq!(
                e.feed(k(name), Mode::Normal),
                Feed::Cmd(Command::SetRegister(Some(name))),
                "\"{name} selects the system clipboard register"
            );
        }
    }

    #[test]
    fn yank_operator_and_paste() {
        assert_eq!(feed("yw"), Feed::Cmd(Command::Yank(1, Motion::WordFwd)));
        assert_eq!(feed("y2w"), Feed::Cmd(Command::Yank(2, Motion::WordFwd)));
        assert_eq!(feed("yy"), Feed::Cmd(Command::Yank(1, Motion::Line)));
        assert_eq!(feed("2yy"), Feed::Cmd(Command::Yank(2, Motion::Line)));
        assert_eq!(feed("yiw"), Feed::Cmd(Command::Yank(1, Motion::InnerWord)));
        assert_eq!(
            feed("p"),
            Feed::Cmd(Command::Paste {
                after: true,
                count: 1,
                move_after: false
            })
        );
        assert_eq!(
            feed("P"),
            Feed::Cmd(Command::Paste {
                after: false,
                count: 1,
                move_after: false
            })
        );
        assert_eq!(
            feed("gp"),
            Feed::Cmd(Command::Paste {
                after: true,
                count: 1,
                move_after: true
            })
        );
        assert_eq!(
            feed("gP"),
            Feed::Cmd(Command::Paste {
                after: false,
                count: 1,
                move_after: true
            })
        );
    }

    #[test]
    fn pending_states() {
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('2'), Mode::Normal), Feed::Pending);
        assert_eq!(e.feed(k('d'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.feed(k('w'), Mode::Normal),
            Feed::Cmd(Command::Delete(2, Motion::WordFwd))
        );
    }

    #[test]
    fn insert_mode_and_ex() {
        let mut e = InputEngine::new();
        assert_eq!(
            e.feed(k('z'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('z'))
        );
        assert_eq!(e.feed(k(':'), Mode::Normal), Feed::Pending);
        assert_eq!(parse_ex("wq"), Ex::SaveQuit);
        assert_eq!(
            parse_ex("trace save t.trace"),
            Ex::SaveTrace("t.trace".into())
        );
        // `:checkhealth` and its `:che` prefix both parse (F-030).
        assert_eq!(parse_ex("checkhealth"), Ex::CheckHealth);
        assert_eq!(parse_ex("che"), Ex::CheckHealth);
        // `:rename {new}` / `:rn {new}` (F-014 LSP rename); a bare verb or a whitespaced name stays Unknown.
        assert_eq!(parse_ex("rename widget"), Ex::Rename("widget".into()));
        assert_eq!(parse_ex("rn widget"), Ex::Rename("widget".into()));
        assert_eq!(
            parse_ex("rename  spaced_out  "),
            Ex::Rename("spaced_out".into())
        );
        assert!(matches!(parse_ex("rename"), Ex::Unknown(_)));
        assert!(matches!(parse_ex("rename a b"), Ex::Unknown(_)));
        assert!(matches!(parse_ex("rn"), Ex::Unknown(_)));
        // `:references` / `:refs` / `:ref` (F-014 LSP references).
        assert_eq!(parse_ex("references"), Ex::References);
        assert_eq!(parse_ex("refs"), Ex::References);
        assert_eq!(parse_ex("ref"), Ex::References);
        // `:codeaction` / `:ca` (F-014 LSP code actions).
        assert_eq!(parse_ex("codeaction"), Ex::CodeAction);
        assert_eq!(parse_ex("ca"), Ex::CodeAction);
        // `:diagnostics` / `:diags` / `:diag` (F-014 diagnostics list); `:d` is still delete.
        assert_eq!(parse_ex("diagnostics"), Ex::Diagnostics);
        assert_eq!(parse_ex("diags"), Ex::Diagnostics);
        assert_eq!(parse_ex("diag"), Ex::Diagnostics);
        assert_eq!(parse_ex("registers"), Ex::Registers);
        assert_eq!(parse_ex("reg"), Ex::Registers);
        assert_eq!(parse_ex("display"), Ex::Registers);
        assert_eq!(parse_ex("marks"), Ex::Marks);
        assert_eq!(parse_ex("jumps"), Ex::Jumps);
        assert_eq!(parse_ex("changes"), Ex::Changes);
        assert_eq!(parse_ex("d"), Ex::Delete(SubRange::CurrentLine));
    }

    /// F-007 multi-buffer ex commands parse: `:enew`, `:ls`, `:bn`/`:bp`, `:b {n}` (with/without space),
    /// `:b#`. A non-numeric `:b` argument and a bare verb that only prefixes a buffer word stay Unknown.
    #[test]
    fn buffer_ex_commands_parse() {
        use crate::input::BufTarget;
        assert_eq!(parse_ex("enew"), Ex::Enew);
        assert_eq!(parse_ex("ene"), Ex::Enew);
        assert_eq!(parse_ex("ls"), Ex::Buffers);
        assert_eq!(parse_ex("buffers"), Ex::Buffers);
        assert_eq!(parse_ex("bn"), Ex::BufferNext);
        assert_eq!(parse_ex("bnext"), Ex::BufferNext);
        assert_eq!(parse_ex("bp"), Ex::BufferPrev);
        assert_eq!(parse_ex("bprevious"), Ex::BufferPrev);
        assert_eq!(parse_ex("b 3"), Ex::Buffer(BufTarget::Number(3)));
        assert_eq!(parse_ex("b2"), Ex::Buffer(BufTarget::Number(2)));
        assert_eq!(parse_ex("buffer 5"), Ex::Buffer(BufTarget::Number(5)));
        assert_eq!(parse_ex("b#"), Ex::Buffer(BufTarget::Alternate));
        assert!(matches!(parse_ex("b foo"), Ex::Unknown(_)));
        assert!(matches!(parse_ex("bx"), Ex::Unknown(_)));
        // `:e {file}` / `:edit {file}` open a file; `:enew`/`:ene` and a bare `:e` are NOT edits.
        assert_eq!(parse_ex("e src/main.rs"), Ex::Edit("src/main.rs".into()));
        assert_eq!(parse_ex("edit a.txt"), Ex::Edit("a.txt".into()));
        assert_eq!(parse_ex("enew"), Ex::Enew);
        assert_eq!(parse_ex("ene"), Ex::Enew);
        assert!(
            matches!(parse_ex("e"), Ex::Unknown(_)),
            "bare :e is not an edit"
        );
    }
}

/// Dot-repeat (`.`): the engine records the last change as a re-parameterizable [`ChangeIntent`] and
/// replays it — the operator/edit at the CURRENT cursor, plus any captured insert text (F-023).
#[cfg(test)]
mod dot_repeat_tests {
    use super::tests::{esc, k};
    use crate::input::*;

    /// Feed a whole sequence, tracking the mode the way the frontend does (a completed command may change
    /// the mode, which the next key must see). Only the outcome matters here; we assert on the LAST feed.
    fn feed_modes(seq: &[KeyEvent]) -> (InputEngine, Feed) {
        let mut e = InputEngine::new();
        let mut mode = Mode::Normal;
        let mut last = Feed::Ignored;
        for key in seq {
            last = e.feed(*key, mode);
            // Track the handful of mode transitions dot-repeat capture depends on.
            match &last {
                Feed::Cmd(Command::EnterInsert)
                | Feed::Cmd(Command::EnterInsertAfter)
                | Feed::Cmd(Command::InsertLineStart)
                | Feed::Cmd(Command::AppendLineEnd)
                | Feed::Cmd(Command::OpenBelow)
                | Feed::Cmd(Command::OpenAbove)
                | Feed::Cmd(Command::Change(..)) => mode = Mode::Insert,
                Feed::Cmd(Command::EnterNormal) => mode = Mode::Normal,
                _ => {}
            }
        }
        (e, last)
    }

    #[test]
    fn dot_with_no_prior_change_is_a_clean_noop() {
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('.'), Mode::Normal), Feed::Ignored);
        assert!(e.last_change.is_none());
    }

    #[test]
    fn operator_change_replays_the_command_at_the_new_cursor() {
        // `dw` records Delete(1, WordFwd); `.` replays exactly that (motion re-run at the new cursor).
        let (_, last) = feed_modes(&[k('d'), k('w'), k('.')]);
        assert_eq!(
            last,
            Feed::Replay(vec![Command::Delete(1, Motion::WordFwd)])
        );
    }

    #[test]
    fn dot_does_not_overwrite_the_record_so_dot_dot_repeats() {
        // `dw..` — the second `.` replays the SAME recorded change, not "repeat of a repeat".
        let (_, last) = feed_modes(&[k('d'), k('w'), k('.'), k('.')]);
        assert_eq!(
            last,
            Feed::Replay(vec![Command::Delete(1, Motion::WordFwd)])
        );
    }

    #[test]
    fn counted_operator_is_recorded_with_its_count() {
        // `d2w` -> Delete(2, WordFwd); `.` repeats with the same count.
        let (_, last) = feed_modes(&[k('d'), k('2'), k('w'), k('.')]);
        assert_eq!(
            last,
            Feed::Replay(vec![Command::Delete(2, Motion::WordFwd)])
        );
    }

    #[test]
    fn single_key_edits_are_dot_repeatable() {
        assert_eq!(
            feed_modes(&[k('x'), k('.')]).1,
            Feed::Replay(vec![Command::DeleteUnder(1)])
        );
        assert_eq!(
            feed_modes(&[k('3'), k('x'), k('.')]).1,
            Feed::Replay(vec![Command::DeleteUnder(3)])
        );
        assert_eq!(
            feed_modes(&[k('>'), k('>'), k('.')]).1,
            Feed::Replay(vec![Command::ShiftRight(1)])
        );
        assert_eq!(
            feed_modes(&[k('d'), k('d'), k('.')]).1,
            Feed::Replay(vec![Command::Delete(1, Motion::Line)])
        );
    }

    #[test]
    fn n_dot_overrides_the_recorded_count() {
        // `3.` after `dw` replays with count 3 (Vim replaces, not multiplies).
        let (_, last) = feed_modes(&[k('d'), k('w'), k('3'), k('.')]);
        assert_eq!(
            last,
            Feed::Replay(vec![Command::Delete(3, Motion::WordFwd)])
        );
        // `2.` after `3x` replaces the 3 with 2.
        let (_, last) = feed_modes(&[k('3'), k('x'), k('2'), k('.')]);
        assert_eq!(last, Feed::Replay(vec![Command::DeleteUnder(2)]));
    }

    #[test]
    fn insert_change_replays_command_and_captured_text() {
        // `ciwFOO<Esc>` -> Change(1, InnerWord) + the inserted chars + the terminating EnterNormal.
        let (_, last) = feed_modes(&[
            k('c'),
            k('i'),
            k('w'),
            k('F'),
            k('O'),
            k('O'),
            esc(),
            k('.'),
        ]);
        assert_eq!(
            last,
            Feed::Replay(vec![
                Command::Change(1, Motion::InnerWord),
                Command::InsertChar('F'),
                Command::InsertChar('O'),
                Command::InsertChar('O'),
                Command::EnterNormal,
            ])
        );
    }

    #[test]
    fn append_insert_is_dot_repeatable_including_text() {
        // `A;<Esc>` then `.` replays AppendLineEnd + the ';' + Esc.
        let (_, last) = feed_modes(&[k('A'), k(';'), esc(), k('.')]);
        assert_eq!(
            last,
            Feed::Replay(vec![
                Command::AppendLineEnd,
                Command::InsertChar(';'),
                Command::EnterNormal,
            ])
        );
    }

    #[test]
    fn yank_is_not_dot_repeatable() {
        // Vim: `yw` is NOT a change; a following `.` has nothing to repeat.
        let mut e = InputEngine::new();
        e.feed(k('y'), Mode::Normal);
        e.feed(k('w'), Mode::Normal);
        assert!(e.last_change.is_none());
        assert_eq!(e.feed(k('.'), Mode::Normal), Feed::Ignored);
    }

    #[test]
    fn named_register_change_replays_with_its_register() {
        // `"ax` records DeleteUnder(1) carrying register a; `.` replays SetRegister(a) THEN the delete.
        let (_, last) = feed_modes(&[k('"'), k('a'), k('x'), k('.')]);
        assert_eq!(
            last,
            Feed::Replay(vec![
                Command::SetRegister(Some('a')),
                Command::DeleteUnder(1),
            ])
        );
    }

    #[test]
    fn unregistered_change_replays_without_a_register() {
        // A plain `x` after a stray-then-consumed register still replays bare (no leading SetRegister).
        let (_, last) = feed_modes(&[k('x'), k('.')]);
        assert_eq!(last, Feed::Replay(vec![Command::DeleteUnder(1)]));
    }

    #[test]
    fn motions_between_changes_do_not_clobber_the_record() {
        // `x` records; then a pure motion `w`; `.` still repeats the `x`.
        let (_, last) = feed_modes(&[k('x'), k('w'), k('.')]);
        assert_eq!(last, Feed::Replay(vec![Command::DeleteUnder(1)]));
    }
}

#[cfg(test)]
mod textobj_tests {
    use super::tests::*;
    use crate::input::*;

    fn pair(open: char, close: char, around: bool) -> Motion {
        Motion::Pair {
            open,
            close,
            around,
        }
    }

    #[test]
    fn text_objects_compose() {
        assert_eq!(
            feed("diw"),
            Feed::Cmd(Command::Delete(1, Motion::InnerWord))
        );
        assert_eq!(
            feed("ciw"),
            Feed::Cmd(Command::Change(1, Motion::InnerWord))
        );
        assert_eq!(feed("daw"), Feed::Cmd(Command::Delete(1, Motion::AWord)));
    }

    #[test]
    fn word_and_bigword_objects() {
        assert_eq!(
            feed("diW"),
            Feed::Cmd(Command::Delete(1, Motion::InnerBigWord))
        );
        assert_eq!(feed("daW"), Feed::Cmd(Command::Delete(1, Motion::ABigWord)));
    }

    #[test]
    fn paragraph_and_sentence_objects() {
        assert_eq!(
            feed("yip"),
            Feed::Cmd(Command::Yank(1, Motion::InnerParagraph))
        );
        assert_eq!(
            feed("dap"),
            Feed::Cmd(Command::Delete(1, Motion::AParagraph))
        );
        assert_eq!(
            feed("dis"),
            Feed::Cmd(Command::Delete(1, Motion::InnerSentence))
        );
        assert_eq!(
            feed("das"),
            Feed::Cmd(Command::Delete(1, Motion::ASentence))
        );
    }

    #[test]
    fn delimiter_pair_objects_and_aliases() {
        // Inner vs around.
        assert_eq!(
            feed("di("),
            Feed::Cmd(Command::Delete(1, pair('(', ')', false)))
        );
        assert_eq!(
            feed("da("),
            Feed::Cmd(Command::Delete(1, pair('(', ')', true)))
        );
        // Closer and `b` alias to the same `()` object as the opener.
        assert_eq!(
            feed("di)"),
            Feed::Cmd(Command::Delete(1, pair('(', ')', false)))
        );
        assert_eq!(
            feed("dab"),
            Feed::Cmd(Command::Delete(1, pair('(', ')', true)))
        );
        // Braces: `{`/`}`/`B` collapse.
        assert_eq!(
            feed("ci{"),
            Feed::Cmd(Command::Change(1, pair('{', '}', false)))
        );
        assert_eq!(
            feed("daB"),
            Feed::Cmd(Command::Delete(1, pair('{', '}', true)))
        );
        // Brackets and angles.
        assert_eq!(
            feed("di["),
            Feed::Cmd(Command::Delete(1, pair('[', ']', false)))
        );
        assert_eq!(
            feed("da]"),
            Feed::Cmd(Command::Delete(1, pair('[', ']', true)))
        );
        assert_eq!(
            feed("di<"),
            Feed::Cmd(Command::Delete(1, pair('<', '>', false)))
        );
    }

    #[test]
    fn quote_objects() {
        assert_eq!(
            feed("da\""),
            Feed::Cmd(Command::Delete(
                1,
                Motion::Quote {
                    ch: '"',
                    around: true
                }
            ))
        );
        assert_eq!(
            feed("ci'"),
            Feed::Cmd(Command::Change(
                1,
                Motion::Quote {
                    ch: '\'',
                    around: false
                }
            ))
        );
        assert_eq!(
            feed("yi`"),
            Feed::Cmd(Command::Yank(
                1,
                Motion::Quote {
                    ch: '`',
                    around: false
                }
            ))
        );
    }

    #[test]
    fn text_object_extends_a_visual_selection() {
        // In Visual, `i`/`a` begin a text object; it completes as a bare `Move` the core turns into a span.
        let mut e = InputEngine::new();
        let vis = Mode::Visual {
            kind: SelectKind::Charwise,
        };
        assert_eq!(e.feed(k('i'), vis), Feed::Pending);
        assert_eq!(
            e.feed(k('w'), vis),
            Feed::Cmd(Command::Move(1, Motion::InnerWord))
        );
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('i'), vis), Feed::Pending);
        assert_eq!(
            e.feed(k('('), vis),
            Feed::Cmd(Command::Move(1, pair('(', ')', false)))
        );
    }

    #[test]
    fn tag_objects_resolve_in_visual() {
        // `it`/`at` now resolve via a core byte scan (see `tag_text_objects` for the operator forms). In
        // Visual they extend the selection like any other text object — `vit` → `Move(1, Tag{inner})`.
        let vis = Mode::Visual {
            kind: SelectKind::Charwise,
        };
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('i'), vis), Feed::Pending);
        assert_eq!(
            e.feed(k('t'), vis),
            Feed::Cmd(Command::Move(1, Motion::Tag { around: false }))
        );
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('a'), vis), Feed::Pending);
        assert_eq!(
            e.feed(k('t'), vis),
            Feed::Cmd(Command::Move(1, Motion::Tag { around: true }))
        );
    }

    #[test]
    fn bare_i_still_enters_insert() {
        let mut e = InputEngine::new();
        assert_eq!(
            e.feed(k('i'), Mode::Normal),
            Feed::Cmd(Command::EnterInsert)
        );
    }
}

#[cfg(test)]
mod search_tests {
    use super::tests::k;
    use crate::input::*;

    #[test]
    fn slash_opens_search_and_n_repeats() {
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('n'), Mode::Normal), Feed::Ignored); // no prior search yet
        assert_eq!(e.feed(k('/'), Mode::Normal), Feed::Pending);
        // Submitting the pattern yields a bare-move Search AND records it for `n`/`N`.
        assert_eq!(
            e.submit_search("foo".into(), false),
            Feed::Cmd(Command::Search {
                op: SearchOp::Move,
                count: 1,
                pattern: "foo".into(),
                backward: false,
            })
        );
        // After a FORWARD search, `n` repeats forward and `N` reverses to backward.
        assert_eq!(
            e.feed(k('n'), Mode::Normal),
            Feed::Cmd(Command::SearchNext("foo".into()))
        );
        assert_eq!(
            e.feed(k('N'), Mode::Normal),
            Feed::Cmd(Command::SearchPrev("foo".into()))
        );
    }

    #[test]
    fn question_opens_backward_search_and_n_repeats_backward() {
        let mut e = InputEngine::new();
        // `?` opens the backward search line; submitting yields a backward Search + records the direction.
        assert_eq!(e.feed(k('?'), Mode::Normal), Feed::Pending);
        assert_eq!(e.cmdline().map(|c| c.0), Some('?'));
        assert_eq!(
            e.submit_search("foo".into(), true),
            Feed::Cmd(Command::Search {
                op: SearchOp::Move,
                count: 1,
                pattern: "foo".into(),
                backward: true,
            })
        );
        // After a BACKWARD search, `n` continues BACKWARD (SearchPrev) and `N` reverses to forward.
        assert_eq!(
            e.feed(k('n'), Mode::Normal),
            Feed::Cmd(Command::SearchPrev("foo".into()))
        );
        assert_eq!(
            e.feed(k('N'), Mode::Normal),
            Feed::Cmd(Command::SearchNext("foo".into()))
        );
    }

    #[test]
    fn operator_folds_into_backward_search() {
        // `d?bar` — the armed operator survives the minibuffer and folds into a backward Search.
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('d'), Mode::Normal), Feed::Pending);
        assert_eq!(e.feed(k('?'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.submit_search("bar".into(), true),
            Feed::Cmd(Command::Search {
                op: SearchOp::Delete,
                count: 1,
                pattern: "bar".into(),
                backward: true,
            })
        );
    }

    #[test]
    fn count_before_question_selects_the_nth_backward_match() {
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('2'), Mode::Normal), Feed::Pending);
        assert_eq!(e.feed(k('?'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.submit_search("foo".into(), true),
            Feed::Cmd(Command::Search {
                op: SearchOp::Move,
                count: 2,
                pattern: "foo".into(),
                backward: true,
            })
        );
    }

    #[test]
    fn empty_backward_search_reuses_last_pattern_backward() {
        // `/foo<CR>` then `?<CR>` reuses "foo" but backward; a following `n` then continues backward.
        let mut e = InputEngine::new();
        e.feed(k('/'), Mode::Normal);
        assert!(matches!(e.submit_search("foo".into(), false), Feed::Cmd(_)));
        e.feed(k('?'), Mode::Normal);
        assert_eq!(
            e.submit_search(String::new(), true),
            Feed::Cmd(Command::Search {
                op: SearchOp::Move,
                count: 1,
                pattern: "foo".into(),
                backward: true,
            })
        );
        assert_eq!(
            e.feed(k('n'), Mode::Normal),
            Feed::Cmd(Command::SearchPrev("foo".into())),
            "empty `?<CR>` reuse flips direction to backward for n"
        );
    }

    #[test]
    fn gn_and_gn_backward_emit_the_search_object_with_the_last_pattern() {
        let mut e = InputEngine::new();
        // Before any search, `gn` finds no pattern and aborts cleanly.
        assert_eq!(e.feed(k('g'), Mode::Normal), Feed::Pending);
        assert_eq!(e.feed(k('n'), Mode::Normal), Feed::Ignored);
        // After a search, bare `gn` = the Move (Visual-select) object; `gN` sets backward.
        e.set_last_search("foo".into(), true);
        assert_eq!(e.feed(k('g'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.feed(k('n'), Mode::Normal),
            Feed::Cmd(Command::SearchObject {
                op: SearchOp::Move,
                count: 1,
                pattern: "foo".into(),
                backward: false,
            })
        );
        e.feed(k('g'), Mode::Normal);
        assert_eq!(
            e.feed(k('N'), Mode::Normal),
            Feed::Cmd(Command::SearchObject {
                op: SearchOp::Move,
                count: 1,
                pattern: "foo".into(),
                backward: true,
            })
        );
    }

    #[test]
    fn operator_and_count_fold_into_gn() {
        let mut e = InputEngine::new();
        e.set_last_search("foo".into(), true);
        // `2cgn` → change the object, count 2 (advance to the 2nd match), pattern baked in.
        assert_eq!(e.feed(k('2'), Mode::Normal), Feed::Pending);
        assert_eq!(e.feed(k('c'), Mode::Normal), Feed::Pending);
        assert_eq!(e.feed(k('g'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.feed(k('n'), Mode::Normal),
            Feed::Cmd(Command::SearchObject {
                op: SearchOp::Change,
                count: 2,
                pattern: "foo".into(),
                backward: false,
            })
        );
        // `dgn` → delete the object.
        assert_eq!(e.feed(k('d'), Mode::Normal), Feed::Pending);
        assert_eq!(e.feed(k('g'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.feed(k('n'), Mode::Normal),
            Feed::Cmd(Command::SearchObject {
                op: SearchOp::Delete,
                count: 1,
                pattern: "foo".into(),
                backward: false,
            })
        );
    }

    #[test]
    fn empty_search_pattern_is_inert() {
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('/'), Mode::Normal), Feed::Pending);
        // No prior search → empty pattern has nothing to reuse, so it aborts.
        assert_eq!(e.submit_search(String::new(), false), Feed::Ignored);
    }

    #[test]
    fn slash_clears_the_transient_axes_but_folds_the_operator_into_the_search() {
        // `/` is a MOTION: the transient count/op/awaiting axes are cleared (so nothing leaks into the
        // minibuffer), but an armed operator/count is CAPTURED for the search to consume — `d/pat`,
        // `2/pat`. This is the fix for the old behaviour that dropped the operator on `/`.
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('d'), Mode::Normal), Feed::Pending);
        assert_eq!(e.feed(k('/'), Mode::Normal), Feed::Pending);
        assert!(
            e.normal.op.is_none() && e.normal.awaiting == Awaiting::Nothing && e.normal.count == 0
        );
        assert_eq!(
            e.submit_search("bar".into(), false),
            Feed::Cmd(Command::Search {
                op: SearchOp::Delete,
                count: 1,
                pattern: "bar".into(),
                backward: false,
            })
        );
    }

    #[test]
    fn count_before_slash_selects_the_nth_match() {
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('2'), Mode::Normal), Feed::Pending);
        assert_eq!(e.feed(k('/'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.submit_search("foo".into(), false),
            Feed::Cmd(Command::Search {
                op: SearchOp::Move,
                count: 2,
                pattern: "foo".into(),
                backward: false,
            })
        );
    }
}

/// Property tests over the input state machine: the hierarchy is explicit, so verify it has no holes —
/// no key sequence leaks a partial command, `awaiting` and the operator axis stay consistent, and `feed`
/// is deterministic. This is the mechanical guard against the "ad-hoc resolution order" class of bug.
#[cfg(test)]
mod state_machine_props {
    use crate::input::*;
    use proptest::prelude::*;

    /// A key drawn from the meaningful command alphabet, plus arbitrary chars (find targets) and specials.
    fn any_key() -> impl Strategy<Value = KeyEvent> {
        let named = "0123456789hjklwbeWBEdcyiaoOAIxfFtT;,vVpPunN$/:gGrJ~%(){}[]<>\"'`sp"
            .chars()
            .collect::<Vec<_>>();
        prop_oneof![
            proptest::sample::select(named)
                .prop_map(|c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)),
            any::<char>().prop_map(|c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)),
            Just(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            Just(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL)),
            Just(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Just(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Just(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
        ]
    }

    fn any_mode() -> impl Strategy<Value = Mode> {
        prop_oneof![
            Just(Mode::Normal),
            Just(Mode::Insert),
            Just(Mode::Replace),
            Just(Mode::Visual {
                kind: SelectKind::Charwise
            }),
            Just(Mode::Visual {
                kind: SelectKind::Linewise
            }),
            Just(Mode::Select {
                kind: SelectKind::Charwise
            }),
            Just(Mode::Select {
                kind: SelectKind::Linewise
            }),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// No key sequence ever leaves a partial command dangling: after any outcome that is not
        /// `Feed::Pending`, every transient axis (count, operator, key-expectation) is cleared. And a
        /// text-object expectation is only ever armed with an operator present. (Never panics — implicit.)
        #[test]
        fn no_pending_state_ever_leaks(steps in prop::collection::vec((any_key(), any_mode()), 0..80)) {
            let mut e = InputEngine::new();
            for (key, mode) in steps {
                let feed = e.feed(key, mode);
                // Orthogonal-axis invariant: a text object is only ever awaited with an operator armed
                // (`diw`) or from a selection mode (`viw`) — never bare in Normal. `mode` is the mode the
                // arming key was fed with, so this checks the arm site directly.
                if let Awaiting::TextObjectChar { .. } = e.normal.awaiting {
                    let in_selection = matches!(mode, Mode::Visual { .. } | Mode::Select { .. });
                    prop_assert!(
                        e.normal.op.is_some() || in_selection,
                        "text object awaited with neither an operator nor a selection"
                    );
                }
                if feed == Feed::Pending {
                    // A pending outcome must correspond to real accumulated state — including the two
                    // Insert-only prefixes (`CTRL-O` one-shot, `CTRL-G u`), which are pending too.
                    let has_state = e.normal.count > 0
                        || e.normal.op.is_some()
                        || e.normal.awaiting != Awaiting::Nothing
                        || !e.activations.is_empty()
                        || e.insert.ctrl_g
                        || e.insert.ctrl_r
                        || e.cmdline.is_some(); // an open command-line namespace is real pending state (F-026)
                    prop_assert!(has_state, "Feed::Pending but the engine is idle");
                } else {
                    prop_assert_eq!(e.normal.count, 0, "count leaked after {:?}", feed);
                    prop_assert!(e.normal.op.is_none(), "operator leaked after {:?}", feed);
                    prop_assert!(e.normal.awaiting == Awaiting::Nothing, "key-expectation leaked after {:?}", feed);
                    prop_assert!(e.activations.is_empty(), "one-shot leaked after {:?}", feed);
                    prop_assert!(!e.insert.ctrl_g, "ctrl-g prefix leaked after {:?}", feed);
                    prop_assert!(!e.insert.ctrl_r, "ctrl-r prefix leaked after {:?}", feed);
                }
            }
        }

        /// `feed` is a pure function of (state, key, mode): two engines fed the same sequence agree at
        /// every step. Determinism is what makes trace replay sound.
        #[test]
        fn feed_is_deterministic(steps in prop::collection::vec((any_key(), any_mode()), 0..40)) {
            let mut a = InputEngine::new();
            let mut b = InputEngine::new();
            for (key, mode) in steps {
                prop_assert_eq!(a.feed(key, mode), b.feed(key, mode));
            }
        }
    }
}

#[cfg(test)]
mod cmdline_tests {
    use super::tests::k;
    use crate::input::*;

    fn special(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn enter() -> KeyEvent {
        special(KeyCode::Enter)
    }

    #[test]
    fn colon_opens_the_namespace_the_engine_owns_the_line_and_cr_executes() {
        // F-026 #1/#2: `:` enters the namespace; the engine (not the UI) owns the buffer; <CR> runs it.
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k(':'), Mode::Normal), Feed::Pending);
        assert_eq!(e.cmdline(), Some((':', "", 0)));
        assert_eq!(e.feed(k('w'), Mode::Normal), Feed::Pending);
        assert_eq!(e.feed(k('q'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.cmdline(),
            Some((':', "wq", 2)),
            "the namespace owns the line buffer + cursor"
        );
        assert_eq!(e.feed(enter(), Mode::Normal), Feed::ExecuteEx("wq".into()));
        assert_eq!(e.cmdline(), None, "<CR> closes the one-shot command line");
    }

    #[test]
    fn slash_search_now_flows_entirely_through_the_engine() {
        // F-026: no ad-hoc UI buffer — `/`, the pattern chars, and <CR> all go through feed().
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('/'), Mode::Normal), Feed::Pending);
        for c in "foo".chars() {
            assert_eq!(e.feed(k(c), Mode::Normal), Feed::Pending);
        }
        assert_eq!(
            e.feed(enter(), Mode::Normal),
            Feed::Cmd(Command::Search {
                op: SearchOp::Move,
                count: 1,
                pattern: "foo".into(),
                backward: false,
            })
        );
        assert_eq!(e.cmdline(), None);
        // And the operator-fold still works because the buffer moved INTO the engine: `d/bar`.
        assert_eq!(e.feed(k('d'), Mode::Normal), Feed::Pending);
        assert_eq!(e.feed(k('/'), Mode::Normal), Feed::Pending);
        for c in "bar".chars() {
            e.feed(k(c), Mode::Normal);
        }
        assert_eq!(
            e.feed(enter(), Mode::Normal),
            Feed::Cmd(Command::Search {
                op: SearchOp::Delete,
                count: 1,
                pattern: "bar".into(),
                backward: false,
            })
        );
    }

    #[test]
    fn esc_aborts_the_command_line_without_executing() {
        let mut e = InputEngine::new();
        e.feed(k(':'), Mode::Normal);
        e.feed(k('x'), Mode::Normal);
        assert_eq!(e.feed(special(KeyCode::Esc), Mode::Normal), Feed::Ignored);
        assert_eq!(e.cmdline(), None, "<Esc> closes the line and runs nothing");
    }

    #[test]
    fn backspace_deletes_back_in_the_owned_buffer() {
        let mut e = InputEngine::new();
        e.feed(k(':'), Mode::Normal);
        e.feed(k('a'), Mode::Normal);
        e.feed(k('b'), Mode::Normal);
        e.feed(special(KeyCode::Backspace), Mode::Normal);
        assert_eq!(e.cmdline(), Some((':', "a", 1)));
    }

    #[test]
    fn gq_enters_ex_mode_reprompts_and_visual_exits() {
        // F-026 #3: `gQ` (only) enters Ex mode; <CR> runs AND re-opens; `:visual` leaves it.
        let mut e = InputEngine::new();
        e.feed(k('g'), Mode::Normal);
        assert_eq!(e.feed(k('Q'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.cmdline(),
            Some((':', "", 0)),
            "gQ opens the Ex command line"
        );
        for c in "set".chars() {
            e.feed(k(c), Mode::Normal);
        }
        assert_eq!(e.feed(enter(), Mode::Normal), Feed::ExecuteEx("set".into()));
        assert_eq!(
            e.cmdline(),
            Some((':', "", 0)),
            "Ex mode re-prompts after <CR>"
        );
        for c in "visual".chars() {
            e.feed(k(c), Mode::Normal);
        }
        assert_eq!(e.feed(enter(), Mode::Normal), Feed::Ignored);
        assert_eq!(e.cmdline(), None, "`:visual` exits Ex mode");
    }

    #[test]
    fn bare_q_is_not_an_ex_mode_key() {
        // At the pinned Neovim revision `Q` is replay-last-register, NOT Ex mode — only `gQ` opens it.
        let mut e = InputEngine::new();
        let out = e.feed(k('Q'), Mode::Normal);
        assert_ne!(
            out,
            Feed::Pending,
            "a bare Q must not open the Ex command line"
        );
        assert_eq!(e.cmdline(), None);
    }
}

#[cfg(test)]
mod namespace_tests {
    use super::tests::k;
    use crate::input::*;

    /// F-003 #4: the `open/overwrite` policy is EXERCISED through the router — a printable key nothing
    /// binds in Replace mode overwrites (via the Replace layer's declared policy), tab-aware in gR.
    #[test]
    fn overwrite_policy_is_exercised_through_the_router() {
        let mut e = InputEngine::new();
        assert_eq!(
            e.feed(k('x'), Mode::Replace),
            Feed::Cmd(Command::ReplaceType('x')),
            "R + printable overwrites (open/overwrite)"
        );
        assert_eq!(
            e.feed(k('y'), Mode::VirtualReplace),
            Feed::Cmd(Command::VirtualReplaceType('y')),
            "gR + printable overwrites tab-aware"
        );
        // The bound keys still resolve through the same layer.
        assert_eq!(
            e.feed(
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                Mode::Replace
            ),
            Feed::Cmd(Command::EnterNormal),
        );
    }

    /// F-003 #4: the `open/insert` and `open/replace-selection` policies are exercised — an unmatched
    /// printable does the namespace's OPEN thing, never falls through to a shared ignore.
    #[test]
    fn insert_and_replace_selection_policies_are_exercised() {
        let mut e = InputEngine::new();
        assert_eq!(
            e.feed(k('z'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('z')),
            "Insert: printable is text (open/insert)"
        );
        let mut e2 = InputEngine::new();
        assert_eq!(
            e2.feed(
                k('q'),
                Mode::Select {
                    kind: SelectKind::Charwise
                }
            ),
            Feed::Cmd(Command::ReplaceSelection('q')),
            "Select: printable replaces the selection (open/replace-selection)"
        );
    }
}

#[cfg(test)]
mod layer_state_tests {
    use super::tests::k;
    use crate::input::*;

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    /// F-003 #5 (KL-OBL-4): the Normal-family grammar layer OWNS its count/operator/awaiting, and that
    /// state DIES when the layer deactivates — it is not carried into another namespace, and it is the
    /// layer's lifecycle that clears it (dropped as `NormalState::default`), not the engine resetting
    /// named fields on a foreign key.
    #[test]
    fn normal_grammar_state_dies_when_the_layer_deactivates() {
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('2'), Mode::Normal), Feed::Pending);
        assert_eq!(e.normal.count, 2, "the Normal layer owns the pending count");
        // `d` consumes the count into the armed operator (Vim moves 2 into `2d`), which the layer
        // still owns — the point is that all of it is Normal-family state.
        assert_eq!(e.feed(k('d'), Mode::Normal), Feed::Pending);
        assert!(
            e.normal.op.is_some(),
            "the Normal layer owns the armed operator"
        );
        // A key arriving in Insert means the Normal family deactivated: its state dies with it.
        let _ = e.feed(k('x'), Mode::Insert);
        assert_eq!(e.normal.count, 0, "count died with the Normal layer");
        assert!(e.normal.op.is_none(), "operator died with the Normal layer");
        assert!(
            e.normal.awaiting == Awaiting::Nothing,
            "key-expectation died with the Normal layer"
        );
    }

    /// F-003 #5/#6: an `i_CTRL-O` one-shot is a return address on the activation stack (KL-OBL-5); a
    /// key arriving back in Normal abandons the pending Insert resume rather than leaking it.
    #[test]
    fn insert_one_shot_activation_is_abandoned_when_insert_deactivates() {
        let mut e = InputEngine::new();
        assert_eq!(e.feed(ctrl('o'), Mode::Insert), Feed::Pending);
        assert!(
            !e.activations.is_empty(),
            "CTRL-O pushed a one-shot activation"
        );
        // The next key arrives back in Normal — the Insert resume address no longer applies.
        let _ = e.feed(k('l'), Mode::Normal);
        assert!(
            e.activations.is_empty(),
            "the pending Insert one-shot was abandoned"
        );
    }

    /// KL-OBL-4 must NOT drop the Normal state during an `i_CTRL-O` one-shot: the one-shot runs a
    /// single Normal command from WITHIN Insert, so the Normal family is momentarily active there and
    /// its count must survive the mode still reading `Insert` (`i<C-o>2l` moves two).
    #[test]
    fn one_shot_keeps_normal_state_alive_inside_insert() {
        let mut e = InputEngine::new();
        assert_eq!(e.feed(ctrl('o'), Mode::Insert), Feed::Pending);
        // Build a count through the Normal grammar while the mode is still Insert.
        assert_eq!(e.feed(k('2'), Mode::Insert), Feed::Pending);
        assert_eq!(
            e.normal.count, 2,
            "the one-shot keeps the Normal count alive inside Insert (not dropped by KL-OBL-4)"
        );
    }

    /// F-003 #6 (KL-OBL-5): `i_CTRL-O` records a return ADDRESS (`resume: Insert`) on the activation
    /// stack — WHENCE control came, which a flat boolean edge cannot express — and the completing
    /// command pops it, resuming Insert. `t_CTRL-\ CTRL-O` is the SAME construct (`resume: Terminal`),
    /// deferred only because no terminal buffers exist yet.
    #[test]
    fn one_shot_records_a_return_address_and_pops_it_on_completion() {
        let mut e = InputEngine::new();
        assert_eq!(e.feed(ctrl('o'), Mode::Insert), Feed::Pending);
        assert_eq!(
            e.activations,
            vec![Suspended { resume: Ns::Insert }],
            "the return address is Insert — whence the one-shot came"
        );
        // One Normal command (`l` = move right) completes the one-shot; its return address pops.
        let out = e.feed(k('l'), Mode::Insert);
        assert!(
            matches!(out, Feed::Cmd(_)),
            "the borrowed Normal command ran"
        );
        assert!(
            e.activations.is_empty(),
            "completion popped the return address, resuming Insert"
        );
    }

    /// F-003 #7 (VS-OBL-1): mode is per-window/per-buffer — the same key resolves through DIFFERENT
    /// namespaces depending on the mode the engine is fed (F-007 gives each View its own mode, and the
    /// frontend feeds the focused View's mode). Here the same engine dispatches one window's Insert
    /// keystroke as text and another window's Normal keystroke as a command.
    #[test]
    fn the_same_key_resolves_by_the_per_window_mode() {
        let mut e = InputEngine::new();
        // Window A is in Insert: `x` is literal text (open/insert).
        assert_eq!(
            e.feed(k('x'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('x'))
        );
        // Window B (same engine, next keystroke) is in Normal: `x` is delete-char, NOT text.
        let out = e.feed(k('x'), Mode::Normal);
        assert_ne!(
            out,
            Feed::Cmd(Command::InsertChar('x')),
            "in Normal the same key is a command, not inserted text"
        );
    }
}

#[cfg(test)]
mod substitute_parse_tests {
    use crate::input::*;

    fn sub(line: &str) -> SubSpec {
        match parse_ex(line) {
            Ex::Substitute(s) => s,
            other => panic!("expected Substitute, got {other:?}"),
        }
    }

    #[test]
    fn plain_substitute_current_line() {
        let s = sub("s/foo/bar/");
        assert_eq!(s.range, SubRange::CurrentLine);
        assert_eq!(s.pattern, "foo");
        assert_eq!(s.replacement, "bar");
        assert!(!s.global && s.ignore_case.is_none() && !s.confirm);
    }

    #[test]
    fn whole_file_global() {
        let s = sub("%s/a/b/g");
        assert_eq!(s.range, SubRange::WholeFile);
        assert!(s.global);
    }

    #[test]
    fn line_range_and_flags() {
        let s = sub("2,5s/a/b/gi");
        assert_eq!(s.range, SubRange::Lines(2, 5));
        assert!(s.global);
        assert_eq!(s.ignore_case, Some(true));
    }

    #[test]
    fn capital_i_forces_case_sensitive_and_c_is_confirm() {
        let s = sub("s/a/b/Ic");
        assert_eq!(s.ignore_case, Some(false));
        assert!(s.confirm);
    }

    #[test]
    fn gdefault_inverts_the_g_flag() {
        // With gdefault ON, a BARE `:s///` is global and `:s///g` is single.
        assert!(parse_substitute("s/a/b/", true).unwrap().global);
        assert!(!parse_substitute("s/a/b/g", true).unwrap().global);
    }

    #[test]
    fn escaped_delimiter_stays_in_the_pattern() {
        let s = sub("s/a\\/b/c/");
        assert_eq!(s.pattern, "a\\/b");
        assert_eq!(s.replacement, "c");
    }

    #[test]
    fn sort_is_not_a_substitute() {
        // `:sort` is its own command (the `o` is not an `:s` delimiter), never a substitute.
        assert!(matches!(parse_ex("sort"), Ex::Sort(_, _)));
        assert!(!matches!(parse_ex("sort"), Ex::Substitute(_)));
    }
}

#[cfg(test)]
mod global_parse_tests {
    use crate::input::*;

    fn glob(line: &str) -> GlobalSpec {
        match parse_ex(line) {
            Ex::Global(g) => g,
            other => panic!("expected Global, got {other:?}"),
        }
    }

    #[test]
    fn global_delete_defaults_to_whole_file() {
        let g = glob("g/foo/d");
        assert_eq!(g.range, SubRange::WholeFile);
        assert_eq!(g.pattern, "foo");
        assert!(!g.negate);
        assert_eq!(g.cmd, GlobalPayload::Core(GlobalCmd::Delete));
    }

    #[test]
    fn bang_and_v_negate() {
        assert!(glob("g!/foo/d").negate);
        assert!(glob("v/foo/d").negate);
        assert!(glob("vglobal/foo/d").negate);
    }

    #[test]
    fn global_substitute_subcommand() {
        let g = glob("g/foo/s/x/y/g");
        assert_eq!(g.pattern, "foo");
        assert_eq!(
            g.cmd,
            GlobalPayload::Core(GlobalCmd::Substitute {
                pattern: "x".into(),
                replacement: "y".into(),
                flags: SubFlags {
                    global: true,
                    ignore_case: None
                },
            })
        );
    }

    #[test]
    fn ranged_global() {
        let g = glob("2,5g/foo/d");
        assert_eq!(g.range, SubRange::Lines(2, 5));
    }

    #[test]
    fn vsplit_is_not_a_global() {
        // `:vsplit` must stay the window command, not be parsed as `:v`-global.
        assert_eq!(parse_ex("vsplit"), Ex::VSplit);
        assert_eq!(parse_ex("vs"), Ex::VSplit);
    }

    #[test]
    fn global_normal_payload_parses() {
        let g = glob("g/foo/normal A;");
        assert_eq!(g.pattern, "foo");
        assert!(!g.negate);
        assert_eq!(
            g.cmd,
            GlobalPayload::Normal {
                bang: false,
                keys: "A;".into(),
            }
        );
        // Only the FIRST space delimits the payload — further spaces are part of the verbatim keys.
        assert_eq!(
            glob("g/foo/normal f x").cmd,
            GlobalPayload::Normal {
                bang: false,
                keys: "f x".into(),
            }
        );
        // Vim's `norm`/`norma` abbreviations resolve the same, and `<>` key-notation stays verbatim.
        assert_eq!(
            glob("g/foo/norm Ihi<Esc>").cmd,
            GlobalPayload::Normal {
                bang: false,
                keys: "Ihi<Esc>".into(),
            }
        );
    }

    #[test]
    fn global_normal_negation_and_bang() {
        // `:v/` and `:g!/` mark the NON-matching lines; the payload `!` (ignore mappings) is parsed.
        assert!(glob("v/foo/normal A;").negate);
        assert!(glob("g!/foo/normal A;").negate);
        assert_eq!(
            glob("g/foo/normal! A;").cmd,
            GlobalPayload::Normal {
                bang: true,
                keys: "A;".into(),
            }
        );
    }

    #[test]
    fn global_normalx_is_not_the_normal_verb() {
        // `normalx` is not `normal x` — with no `d`/`s`/`normal` payload the whole `:g` line is Unknown.
        assert!(matches!(parse_ex("g/foo/normalx"), Ex::Unknown(_)));
        // A bare `normal` (no delimiting space/keys) is likewise not a valid payload.
        assert!(matches!(parse_ex("g/foo/normal"), Ex::Unknown(_)));
    }
}

#[cfg(test)]
mod nohighlight_parse_tests {
    use crate::input::*;

    #[test]
    fn noh_variants_parse() {
        assert_eq!(parse_ex("noh"), Ex::NoHighlight);
        assert_eq!(parse_ex("nohl"), Ex::NoHighlight);
        assert_eq!(parse_ex("nohlsearch"), Ex::NoHighlight);
    }
}

#[cfg(test)]
mod binding_label_tests {
    use crate::input::*;

    /// F-004 #2: a command with a STATIC layer binding reports it; a grammar/ex command (no layer
    /// binding) reports None — the deliberate MVP scope (static layer bindings only).
    #[test]
    fn binding_label_reports_static_layer_bindings_only() {
        let e = InputEngine::new();
        // The Insert layer binds Esc -> EnterNormal, Enter -> InsertNewline, Backspace -> DeleteBack.
        assert_eq!(
            e.binding_label(&Command::EnterNormal).as_deref(),
            Some("Esc")
        );
        assert_eq!(
            e.binding_label(&Command::InsertNewline).as_deref(),
            Some("CR")
        );
        assert_eq!(e.binding_label(&Command::DeleteBack).as_deref(), Some("BS"));
        // Undo (`u`) is Normal grammar, not a layer binding, so it has no static binding here.
        assert_eq!(e.binding_label(&Command::Undo), None);
        assert_eq!(e.binding_label(&Command::Save), None);
    }

    /// Arm an operator prefix, then resolve a viewport screen-motion (`H`/`M`/`L`) to the ABSOLUTE `line`
    /// the frontend computed — exercises `screen_op`, the operator-composition seam for `dH`/`yL`/`>H`/… .
    fn op_to_line(prefix: &str, line: u32) -> Feed {
        let mut e = InputEngine::new();
        for c in prefix.chars() {
            e.feed(
                KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
                Mode::Normal,
            );
        }
        e.screen_op(line)
    }

    /// The OPERATOR forms of `H`/`M`/`L` (`:help H`): any pending operator composes with the viewport-resolved
    /// target line over `GotoLine` (LINEWISE in core). Ground truth verified against nvim v0.12.4: e.g. `dH`
    /// from a mid-screen line deletes up THROUGH the top visible line inclusive. The frontend hands the
    /// resolved line in; here we assert the command `screen_op` builds for each operator.
    #[test]
    fn screen_motions_compose_with_operators() {
        // No operator armed: plain `H`/`M`/`L` stays a bare cursor move (preserved behavior).
        assert_eq!(
            op_to_line("", 4),
            Feed::Cmd(Command::Move(4, Motion::GotoLine))
        );
        // `dH` / `cM` / `yL` — delete/change/yank LINEWISE through the resolved line. Direction is handled by
        // the core `GotoLine` operator range (`[min,max]` of cursor and target), so the same command serves
        // cursor-above-target and cursor-below-target.
        assert_eq!(
            op_to_line("d", 4),
            Feed::Cmd(Command::Delete(4, Motion::GotoLine))
        );
        assert_eq!(
            op_to_line("c", 5),
            Feed::Cmd(Command::Change(5, Motion::GotoLine))
        );
        assert_eq!(
            op_to_line("y", 1),
            Feed::Cmd(Command::Yank(1, Motion::GotoLine))
        );
        // `>H` / `<L` / `=M` — shift and reindent compose too (always linewise).
        assert_eq!(
            op_to_line(">", 3),
            Feed::Cmd(Command::ShiftMotion {
                left: false,
                count: 3,
                motion: Motion::GotoLine,
            })
        );
        assert_eq!(
            op_to_line("<", 8),
            Feed::Cmd(Command::ShiftMotion {
                left: true,
                count: 8,
                motion: Motion::GotoLine,
            })
        );
        assert_eq!(
            op_to_line("=", 7),
            Feed::Cmd(Command::Reindent {
                count: 7,
                motion: Motion::GotoLine,
            })
        );
        // `gUH` / `guL` / `g~M` — the case operators compose as well.
        assert_eq!(
            op_to_line("gU", 2),
            Feed::Cmd(Command::CaseMotion {
                count: 2,
                motion: Motion::GotoLine,
                case: WordCase::Upcase,
            })
        );
        assert_eq!(
            op_to_line("gu", 2),
            Feed::Cmd(Command::CaseMotion {
                count: 2,
                motion: Motion::GotoLine,
                case: WordCase::Downcase,
            })
        );
        assert_eq!(
            op_to_line("g~", 6),
            Feed::Cmd(Command::CaseMotion {
                count: 6,
                motion: Motion::GotoLine,
                case: WordCase::Toggle,
            })
        );
    }

    /// The effective count the frontend uses to resolve the `H`/`M`/`L` TARGET LINE from the viewport, and
    /// the pending-operator flag it uses to drop 'scrolloff' under an operator. Count multiplication matches
    /// nvim: `2H` = 2, `3d2H` = 6 (op-count times motion-count), `3dH` = 3, bare `H`/`dH` = 0/1.
    #[test]
    fn screen_count_and_has_op_track_pending_state() {
        fn state(prefix: &str) -> (u32, bool) {
            let mut e = InputEngine::new();
            for c in prefix.chars() {
                e.feed(
                    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
                    Mode::Normal,
                );
            }
            (e.screen_count(), e.has_op())
        }
        // No count, no operator: raw count 0 (viewport resolver treats it as line 1), no op → scrolloff kept.
        assert_eq!(state(""), (0, false));
        // Bare count (plain `2H`): count 2, no op.
        assert_eq!(state("2"), (2, false));
        // Operator only (`dH`): effective count 1, op armed → scrolloff dropped.
        assert_eq!(state("d"), (1, true));
        // Count after the operator (`d2H`): 2.
        assert_eq!(state("d2"), (2, true));
        // Count before the operator (`3dH`): 3.
        assert_eq!(state("3d"), (3, true));
        // Both multiply (`3d2H` = 6H): 6.
        assert_eq!(state("3d2"), (6, true));
    }

    // ---- Command-line & search history recall (`:help cmdline-history`) ------------------------------
    // Integration tests driving the `:`/`/` prompt state machine through the full engine. Behaviour is
    // matched to nvim v0.12.4 (see apps/tui/src/input/history.rs for the ground-truth probe results).

    use super::tests::k;

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }
    fn up() -> KeyEvent {
        KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)
    }
    fn down() -> KeyEvent {
        KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)
    }
    fn enter() -> KeyEvent {
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
    }

    /// Type `line` into an already-open prompt and accept it with `<CR>`.
    fn accept_line(e: &mut InputEngine, line: &str) {
        for c in line.chars() {
            e.feed(k(c), Mode::Normal);
        }
        e.feed(enter(), Mode::Normal);
    }

    /// The current command-line buffer text (`None` when no prompt is open).
    fn line(e: &InputEngine) -> Option<String> {
        e.cmdline().map(|(_, t, _)| t.to_string())
    }

    #[test]
    fn ex_history_up_recalls_newest_then_older() {
        // nvim: after `:set foo<CR>` `:echo x<CR>`, open `:` and press <Up> twice → 'echo x', 'set foo'.
        let mut e = InputEngine::new();
        e.feed(k(':'), Mode::Normal);
        accept_line(&mut e, "set foo");
        e.feed(k(':'), Mode::Normal);
        accept_line(&mut e, "echo x");
        e.feed(k(':'), Mode::Normal);
        e.feed(up(), Mode::Normal);
        assert_eq!(line(&e).as_deref(), Some("echo x"));
        e.feed(up(), Mode::Normal);
        assert_eq!(line(&e).as_deref(), Some("set foo"));
        // No older entry: the walk stays put.
        e.feed(up(), Mode::Normal);
        assert_eq!(line(&e).as_deref(), Some("set foo"));
    }

    #[test]
    fn ex_history_up_prefix_filters() {
        // nvim: `:e` + <Up> recalls only `:e…` entries, newest first.
        let mut e = InputEngine::new();
        for cmd in ["edit a", "echo y", "edit b"] {
            e.feed(k(':'), Mode::Normal);
            accept_line(&mut e, cmd);
        }
        e.feed(k(':'), Mode::Normal);
        e.feed(k('e'), Mode::Normal);
        e.feed(k('d'), Mode::Normal); // draft = "ed"
        e.feed(up(), Mode::Normal);
        assert_eq!(line(&e).as_deref(), Some("edit b"));
        e.feed(up(), Mode::Normal);
        assert_eq!(line(&e).as_deref(), Some("edit a")); // 'echo y' skipped by the "ed" prefix
    }

    #[test]
    fn ctrl_p_walks_raw_history_unfiltered() {
        // nvim-VERIFIED distinction: <C-p> ignores the typed prefix. `:e` + <C-p> → 'edit b'
        // (raw newest), <C-p> again → 'echo y' (raw next, NOT prefix-filtered).
        let mut e = InputEngine::new();
        for cmd in ["edit a", "echo y", "edit b"] {
            e.feed(k(':'), Mode::Normal);
            accept_line(&mut e, cmd);
        }
        e.feed(k(':'), Mode::Normal);
        e.feed(k('e'), Mode::Normal); // draft = "e"
        e.feed(ctrl('p'), Mode::Normal);
        assert_eq!(line(&e).as_deref(), Some("edit b"));
        e.feed(ctrl('p'), Mode::Normal);
        assert_eq!(line(&e).as_deref(), Some("echo y"));
    }

    #[test]
    fn down_restores_typed_draft() {
        // nvim: `:e` <Up> <Down> → 'e' (the draft returns); <C-n> mirrors <Down>.
        let mut e = InputEngine::new();
        e.feed(k(':'), Mode::Normal);
        accept_line(&mut e, "edit b");
        e.feed(k(':'), Mode::Normal);
        e.feed(k('e'), Mode::Normal);
        e.feed(up(), Mode::Normal);
        assert_eq!(line(&e).as_deref(), Some("edit b"));
        e.feed(down(), Mode::Normal);
        assert_eq!(line(&e).as_deref(), Some("e"));
        e.feed(ctrl('n'), Mode::Normal); // already at draft → no-op
        assert_eq!(line(&e).as_deref(), Some("e"));
    }

    #[test]
    fn search_history_is_separate_from_ex() {
        // nvim keeps `:` and `/` histories apart; `/` and `?` share the search ring.
        let mut e = InputEngine::new();
        e.feed(k(':'), Mode::Normal);
        accept_line(&mut e, "foo");
        e.feed(k('/'), Mode::Normal);
        accept_line(&mut e, "bar");
        // The `:` prompt recalls only ex history.
        e.feed(k(':'), Mode::Normal);
        e.feed(up(), Mode::Normal);
        assert_eq!(line(&e).as_deref(), Some("foo"));
        e.feed(enter(), Mode::Normal);
        // The `/` prompt recalls only search history.
        e.feed(k('/'), Mode::Normal);
        e.feed(up(), Mode::Normal);
        assert_eq!(line(&e).as_deref(), Some("bar"));
        // `?` shares the same search ring.
        e.feed(enter(), Mode::Normal);
        e.feed(k('?'), Mode::Normal);
        e.feed(up(), Mode::Normal);
        assert_eq!(line(&e).as_deref(), Some("bar"));
    }

    #[test]
    fn immediate_repeat_is_deduped_in_history() {
        // nvim: re-entering an identical ex line moves it to the most-recent slot (single entry).
        let mut e = InputEngine::new();
        for cmd in ["foo", "bar", "foo"] {
            e.feed(k(':'), Mode::Normal);
            accept_line(&mut e, cmd);
        }
        e.feed(k(':'), Mode::Normal);
        e.feed(up(), Mode::Normal);
        assert_eq!(line(&e).as_deref(), Some("foo"));
        e.feed(up(), Mode::Normal);
        assert_eq!(line(&e).as_deref(), Some("bar"));
        // Only two distinct entries remain — a third <Up> stays on the oldest.
        e.feed(up(), Mode::Normal);
        assert_eq!(line(&e).as_deref(), Some("bar"));
    }

    #[test]
    fn empty_line_is_not_recorded() {
        // An accepted empty `:` line stores nothing; a later <Up> finds an earlier real entry.
        let mut e = InputEngine::new();
        e.feed(k(':'), Mode::Normal);
        accept_line(&mut e, "real");
        e.feed(k(':'), Mode::Normal);
        accept_line(&mut e, ""); // empty → not stored
        e.feed(k(':'), Mode::Normal);
        e.feed(up(), Mode::Normal);
        assert_eq!(line(&e).as_deref(), Some("real"));
    }

    // ---- Command-line window (`:help cmdwin`, q: q/ q?) -----------------------------------------------
    // The engine side of the reduced list-overlay slice. `q:`/`q/`/`q?` are routed to `open_cmdwin` by the
    // macro layer (keys.rs, tested there); here we open the window directly and drive its keys. Behaviour
    // mirrors nvim v0.12.4: the list is the history ring with a trailing empty line the cursor starts on;
    // `<CR>` runs the selected line through the SAME ex/search dispatch the `:`/`/` prompt uses; `<Esc>` /
    // `<C-c>` close without running.

    fn ctrl_c() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
    }
    fn esc() -> KeyEvent {
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
    }

    #[test]
    fn cmdwin_cr_executes_the_selected_ex_line_and_closes() {
        let mut e = InputEngine::new();
        for cmd in ["set nu", "write"] {
            e.feed(k(':'), Mode::Normal);
            accept_line(&mut e, cmd);
        }
        e.open_cmdwin(':');
        assert_eq!(e.cmdwin(), Some(':'), "the window is open over the ex ring");
        // The cursor starts on the empty last line; running it is a no-op close (Vim).
        assert_eq!(e.feed(enter(), Mode::Normal), Feed::Ignored);
        assert_eq!(e.cmdwin(), None, "<CR> on the empty line just closes");
        // Re-open and navigate up to the newest entry, then run it.
        e.open_cmdwin(':');
        e.feed(k('k'), Mode::Normal); // `k` = up → "write"
        assert_eq!(
            e.feed(enter(), Mode::Normal),
            Feed::ExecuteEx("write".into()),
            "the selected historical ex line executes"
        );
        assert_eq!(e.cmdwin(), None, "the window closes after running");
        // The run line was recorded in the ex ring via the same accept path a `:` prompt uses.
        e.feed(k(':'), Mode::Normal);
        e.feed(up(), Mode::Normal);
        assert_eq!(line(&e).as_deref(), Some("write"));
    }

    #[test]
    fn cmdwin_navigates_with_j_k_and_arrows() {
        let mut e = InputEngine::new();
        for cmd in ["one", "two", "three"] {
            e.feed(k(':'), Mode::Normal);
            accept_line(&mut e, cmd);
        }
        e.open_cmdwin(':');
        // rows(): oldest first, then the empty selected last line (4 rows).
        let rows = e.cmdwin_rows();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].0, "one");
        assert!(rows[3].1, "the empty last row is selected initially");
        // `k`/<Up> move toward older; `j`/<Down> back toward the empty line.
        e.feed(up(), Mode::Normal); // → three
        e.feed(k('k'), Mode::Normal); // → two
        assert_eq!(e.feed(enter(), Mode::Normal), Feed::ExecuteEx("two".into()));
    }

    #[test]
    fn cmdwin_esc_and_ctrl_c_close_without_executing() {
        let mut e = InputEngine::new();
        e.feed(k(':'), Mode::Normal);
        accept_line(&mut e, "quitcmd");
        // <Esc> closes without running.
        e.open_cmdwin(':');
        e.feed(up(), Mode::Normal); // select "quitcmd"
        assert_eq!(e.feed(esc(), Mode::Normal), Feed::Ignored);
        assert_eq!(e.cmdwin(), None, "<Esc> closes the window");
        // <C-c> likewise closes without running.
        e.open_cmdwin(':');
        e.feed(up(), Mode::Normal);
        assert_eq!(e.feed(ctrl_c(), Mode::Normal), Feed::Ignored);
        assert_eq!(e.cmdwin(), None, "<C-c> closes the window");
    }

    #[test]
    fn cmdwin_search_line_runs_as_a_search() {
        let mut e = InputEngine::new();
        // Populate the SEARCH ring with a `/` line.
        e.feed(k('/'), Mode::Normal);
        accept_line(&mut e, "foo");
        // `q/` mirrors the search ring; running the line performs the search (folds through submit_search).
        e.open_cmdwin('/');
        e.feed(k('k'), Mode::Normal); // select "foo"
        assert_eq!(
            e.feed(enter(), Mode::Normal),
            Feed::Cmd(Command::Search {
                op: SearchOp::Move,
                count: 1,
                pattern: "foo".into(),
                backward: false,
            }),
            "q/ + <CR> runs the chosen pattern as a forward search"
        );
        assert_eq!(e.cmdwin(), None);
    }

    #[test]
    fn cmdwin_q_question_searches_backward() {
        let mut e = InputEngine::new();
        e.feed(k('/'), Mode::Normal);
        accept_line(&mut e, "bar");
        // `q?` mirrors the same shared search ring but runs the line BACKWARD.
        e.open_cmdwin('?');
        e.feed(k('k'), Mode::Normal);
        assert_eq!(
            e.feed(enter(), Mode::Normal),
            Feed::Cmd(Command::Search {
                op: SearchOp::Move,
                count: 1,
                pattern: "bar".into(),
                backward: true,
            })
        );
    }
}
