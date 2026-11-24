use super::{ctrl, fc, feed, k};
use crate::input::*;

#[test]
fn bare_motions_and_counts() {
    assert_eq!(feed("w"), Feed::Cmd(Command::Move(1, Motion::WordFwd)));
    assert_eq!(feed("3w"), Feed::Cmd(Command::Move(3, Motion::WordFwd)));
    assert_eq!(feed("l"), Feed::Cmd(Command::Move(1, Motion::Right)));
}

/// #899: a long count used to overflow `u32` (`saturating_mul(10) + d`, and `op * motion`) — a panic
/// in debug, a silent wrap in release. Counts now clamp at Neovim's 999999999, which is what
/// `nvim --clean` reports as `v:count` for both `99999999999Q` and `99999y99999Q` (v0.12.6).
#[test]
fn huge_counts_clamp_to_the_neovim_ceiling() {
    assert_eq!(
        feed("99999999999w"),
        Feed::Cmd(Command::Move(MAX_COUNT, Motion::WordFwd))
    );
    assert_eq!(
        feed("4294967299w"),
        Feed::Cmd(Command::Move(MAX_COUNT, Motion::WordFwd))
    );
    assert_eq!(
        feed("99999d99999w"),
        Feed::Cmd(Command::Delete(MAX_COUNT, Motion::WordFwd))
    );
    // A `0` digit at the ceiling (the `count > 0` arm) also stays clamped.
    assert_eq!(
        feed("9999999990w"),
        Feed::Cmd(Command::Move(MAX_COUNT, Motion::WordFwd))
    );
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
fn gm_middle_of_line_is_a_percentage_motion() {
    // Bare `gM` = 50% of the line (the count is a percentage, not a repeat); `{count}gM` uses the count.
    assert_eq!(feed("gM"), Feed::Cmd(Command::Move(50, Motion::MidLine)));
    assert_eq!(feed("25gM"), Feed::Cmd(Command::Move(25, Motion::MidLine)));
    assert_eq!(
        feed("100gM"),
        Feed::Cmd(Command::Move(100, Motion::MidLine))
    );
    // Operator-aware: bare `dgM` deletes to the middle (50%), `d25gM` to a quarter.
    assert_eq!(feed("dgM"), Feed::Cmd(Command::Delete(50, Motion::MidLine)));
    assert_eq!(
        feed("d25gM"),
        Feed::Cmd(Command::Delete(25, Motion::MidLine))
    );
    assert_eq!(feed("ygM"), Feed::Cmd(Command::Yank(50, Motion::MidLine)));
}

#[test]
fn g_prefixed_screen_line_motions_go_to_the_frontend() {
    // `gj`/`gk`/`g0`/`g^`/`gm`/`g$` count in the window's screen rows (#615), which the engine does not
    // know: it hands them over with the folded count and keeps the operator armed.
    use crate::input::ScreenMotion::*;
    let sm = |motion, count| Feed::ScreenMotion { motion, count };
    assert_eq!(feed("gj"), sm(Down, 1));
    assert_eq!(feed("gk"), sm(Up, 1));
    assert_eq!(feed("g0"), sm(Start, 1));
    assert_eq!(feed("g^"), sm(FirstNonBlank, 1));
    assert_eq!(feed("gm"), sm(Middle, 1));
    assert_eq!(feed("g$"), sm(End, 1));
    assert_eq!(feed("3gj"), sm(Down, 3));
    assert_eq!(feed("2d3gk"), sm(Up, 6));
    // The frontend finishes it: the operator over the byte target (`go`'s rules), or `j`/`k` without
    // 'wrap'.
    let mut e = InputEngine::new();
    for c in "dgj".chars() {
        e.feed(k(c), Mode::Normal);
    }
    assert_eq!(
        e.screen_motion_to(9),
        Feed::Cmd(Command::Delete(10, Motion::GotoByte))
    );
    for c in "2dgj".chars() {
        e.feed(k(c), Mode::Normal);
    }
    assert_eq!(
        e.screen_motion_lines(true),
        Feed::Cmd(Command::Delete(2, Motion::Down))
    );
    for c in "dgk".chars() {
        e.feed(k(c), Mode::Normal);
    }
    assert_eq!(
        e.screen_motion_fail(),
        Feed::Beep {
            clear_register: false
        }
    );
    // The failed motion dropped the operator: `x` is `x` again.
    assert_eq!(
        e.feed(k('x'), Mode::Normal),
        Feed::Cmd(Command::DeleteUnder(1))
    );
}

#[test]
fn dot_after_an_operator_over_a_screen_motion_resolves_it_again() {
    // #1539: nvim's redo buffer holds `dgj`, not the span `dgj` deleted, so `.` hands the screen motion
    // back to the frontend (with the operator resolving as armed) and deletes over the NEW span.
    use crate::input::ScreenMotion::*;
    let mut e = InputEngine::new();
    for c in "3dgk".chars() {
        e.feed(k(c), Mode::Normal);
    }
    assert_eq!(
        e.screen_motion_to(9),
        Feed::Cmd(Command::Delete(10, Motion::GotoByte))
    );
    assert!(!e.has_op());
    assert_eq!(
        e.feed(k('.'), Mode::Normal),
        Feed::ScreenMotion {
            motion: Up,
            count: 3
        }
    );
    assert!(e.has_op(), "`.` resolves the motion as an operator's");
    assert_eq!(
        e.screen_motion_to(4),
        Feed::Replay(vec![Command::Delete(5, Motion::GotoByte)])
    );
    assert!(!e.has_op());
    // `2.` takes the count; `j` / `k` without 'wrap' replay as the linewise motion with it.
    assert_eq!(e.feed(k('2'), Mode::Normal), Feed::Pending,);
    assert_eq!(
        e.feed(k('.'), Mode::Normal),
        Feed::ScreenMotion {
            motion: Up,
            count: 2
        }
    );
    assert_eq!(
        e.screen_motion_lines(false),
        Feed::Replay(vec![Command::Delete(2, Motion::Up)])
    );
    // A `.` whose motion cannot move beeps and keeps the change.
    e.feed(k('.'), Mode::Normal);
    assert!(matches!(e.screen_motion_fail(), Feed::Beep { .. }));
    assert_eq!(
        e.feed(k('.'), Mode::Normal),
        Feed::ScreenMotion {
            motion: Up,
            count: 3
        }
    );
    assert_eq!(
        e.screen_motion_to(0),
        Feed::Replay(vec![Command::Delete(1, Motion::GotoByte)])
    );

    // A change over one replays its typed text too; a yank over one is no change.
    let mut e = InputEngine::new();
    for c in "xcgjX".chars() {
        let mode = if c == 'X' { Mode::Insert } else { Mode::Normal };
        if c == 'X' {
            assert_eq!(
                e.screen_motion_to(7),
                Feed::Cmd(Command::Change(8, Motion::GotoByte))
            );
        }
        e.feed(k(c), mode);
    }
    e.feed(
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        Mode::Insert,
    );
    for c in "ygj".chars() {
        e.feed(k(c), Mode::Normal);
    }
    assert_eq!(
        e.screen_motion_to(3),
        Feed::Cmd(Command::Yank(4, Motion::GotoByte))
    );
    assert_eq!(
        e.feed(k('.'), Mode::Normal),
        Feed::ScreenMotion {
            motion: Down,
            count: 1
        }
    );
    assert_eq!(
        e.screen_motion_to(2),
        Feed::Replay(vec![
            Command::Change(3, Motion::GotoByte),
            Command::InsertChar('X'),
            Command::EnterNormal,
        ])
    );
}

#[test]
fn a_backward_inclusive_screen_motion_takes_the_cursor_char() {
    // #1576: `dgk` after `$` is inclusive and goes back, so nvim's inclusive end is the cursor: the
    // delete is forced charwise (`o_v`), which takes the cursor's char. `dvgk` toggles it back.
    use ruse_core::{ForcedWise, OpKind};
    let mut e = InputEngine::new();
    for c in "dgk".chars() {
        e.feed(k(c), Mode::Normal);
    }
    assert_eq!(
        e.screen_motion_back_inclusive(79),
        Feed::Cmd(Command::OpForced {
            op: OpKind::Delete,
            count: 80,
            motion: Motion::GotoByte,
            wise: ForcedWise::Charwise,
        })
    );
    // The change is recorded without the force: `.` resolving an exclusive `gk` is a plain delete.
    e.feed(k('.'), Mode::Normal);
    assert_eq!(
        e.screen_motion_to(9),
        Feed::Replay(vec![Command::Delete(10, Motion::GotoByte)])
    );
    e.feed(k('.'), Mode::Normal);
    assert_eq!(
        e.screen_motion_back_inclusive(4),
        Feed::Replay(vec![Command::OpForced {
            op: OpKind::Delete,
            count: 5,
            motion: Motion::GotoByte,
            wise: ForcedWise::Charwise,
        }])
    );
    for c in "dvgk".chars() {
        e.feed(k(c), Mode::Normal);
    }
    assert_eq!(
        e.screen_motion_back_inclusive(79),
        Feed::Cmd(Command::Delete(80, Motion::GotoByte))
    );
    // A bare `gk` is the plain move.
    for c in "gk".chars() {
        e.feed(k(c), Mode::Normal);
    }
    assert_eq!(
        e.screen_motion_back_inclusive(3),
        Feed::Cmd(Command::Move(4, Motion::GotoByte))
    );
}

#[test]
fn a_backward_inclusive_screen_motion_takes_the_cursor_char_under_a_case_operator() {
    // #1595: `gUgk` after `$` is forced charwise like `dgk`; `gUvgk` toggles it back.
    use ruse_core::{ForcedWise, WordCase};
    let case = |wise| {
        Feed::Cmd(Command::CaseMotion {
            count: 80,
            motion: Motion::GotoByte,
            case: WordCase::Upcase,
            wise,
        })
    };
    let mut e = InputEngine::new();
    for c in "gUgk".chars() {
        e.feed(k(c), Mode::Normal);
    }
    assert_eq!(
        e.screen_motion_back_inclusive(79),
        case(Some(ForcedWise::Charwise))
    );
    for c in "gUvgk".chars() {
        e.feed(k(c), Mode::Normal);
    }
    assert_eq!(e.screen_motion_back_inclusive(79), case(None));
}

#[test]
fn case_operators_take_a_forced_wise() {
    // #1595: `gUvj` / `guVe` / `g~<C-v>j` carry the forced wise like `dvj`.
    use ruse_core::{ForcedWise, WordCase};
    let case = |motion, case, wise| {
        Feed::Cmd(Command::CaseMotion {
            count: 1,
            motion,
            case,
            wise: Some(wise),
        })
    };
    assert_eq!(
        feed("gUvj"),
        case(Motion::Down, WordCase::Upcase, ForcedWise::Charwise)
    );
    assert_eq!(
        feed("guVe"),
        case(Motion::WordEnd, WordCase::Downcase, ForcedWise::Linewise)
    );
    let mut e = InputEngine::new();
    for c in "g~".chars() {
        e.feed(k(c), Mode::Normal);
    }
    e.feed(ctrl('v'), Mode::Normal);
    assert_eq!(
        e.feed(k('j'), Mode::Normal),
        case(Motion::Down, WordCase::Toggle, ForcedWise::Blockwise)
    );
}

#[test]
fn dot_after_an_operator_over_h_m_l_resolves_the_line_again() {
    // #1577: the frontend finishes `H` / `M` / `L` through `screen_op`, outside `feed`; the change it
    // finishes is recorded with the viewport motion, so `.` hands the motion back to resolve the line in
    // the window as it is then (nvim's redo buffer holds `dL`, not the lines it deleted).
    use crate::input::ScreenMotion::*;
    let mut e = InputEngine::new();
    for c in "3d".chars() {
        e.feed(k(c), Mode::Normal);
    }
    let count = e.screen_count();
    assert_eq!(count, 3);
    assert_eq!(
        e.screen_op(ViewLow, count, 21),
        Feed::Cmd(Command::Delete(21, Motion::GotoLine))
    );
    assert!(!e.has_op());
    assert_eq!(
        e.feed(k('.'), Mode::Normal),
        Feed::ScreenMotion {
            motion: ViewLow,
            count: 3
        }
    );
    assert!(e.has_op(), "`.` resolves the line as an operator's");
    assert_eq!(
        e.screen_op(ViewLow, 3, 19),
        Feed::Replay(vec![Command::Delete(19, Motion::GotoLine)])
    );
    // `2.` takes the count.
    e.feed(k('2'), Mode::Normal);
    assert_eq!(
        e.feed(k('.'), Mode::Normal),
        Feed::ScreenMotion {
            motion: ViewLow,
            count: 2
        }
    );
    e.screen_op(ViewLow, 2, 20);

    // A change over one replays its typed text; a bare `M` is no change, so `.` still repeats `x`.
    let mut e = InputEngine::new();
    e.feed(k('c'), Mode::Normal);
    assert_eq!(
        e.screen_op(ViewHigh, 1, 1),
        Feed::Cmd(Command::Change(1, Motion::GotoLine))
    );
    e.feed(k('y'), Mode::Insert);
    e.feed(
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        Mode::Insert,
    );
    assert_eq!(
        e.feed(k('.'), Mode::Normal),
        Feed::ScreenMotion {
            motion: ViewHigh,
            count: 1
        }
    );
    assert_eq!(
        e.screen_op(ViewHigh, 1, 40),
        Feed::Replay(vec![
            Command::Change(40, Motion::GotoLine),
            Command::InsertChar('y'),
            Command::EnterNormal,
        ])
    );
    e.feed(k('x'), Mode::Normal);
    assert_eq!(
        e.screen_op(ViewMiddle, 0, 12),
        Feed::Cmd(Command::Move(12, Motion::GotoLine))
    );
    assert!(matches!(e.feed(k('.'), Mode::Normal), Feed::Replay(_)));
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
            whole_word: true,
            op: SearchOp::Move,
            count: 1
        })
    );
    assert_eq!(
        feed("#"),
        Feed::Cmd(Command::SearchWordUnder {
            forward: false,
            whole_word: true,
            op: SearchOp::Move,
            count: 1
        })
    );
    // `g*` / `g#` — match anywhere (no word boundaries).
    assert_eq!(
        feed("3g*"),
        Feed::Cmd(Command::SearchWordUnder {
            forward: true,
            whole_word: false,
            op: SearchOp::Move,
            count: 3
        })
    );
    assert_eq!(
        feed("g#"),
        Feed::Cmd(Command::SearchWordUnder {
            forward: false,
            whole_word: false,
            op: SearchOp::Move,
            count: 1
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
fn insert_copy_char_emits_insertchar_or_ignores() {
    // `i_CTRL-E`/`i_CTRL-Y` are frontend-resolved: the engine takes the already-looked-up char and
    // emits a concrete `InsertChar`, or `Ignored` when the adjacent line has no char at that column.
    let mut e = InputEngine::new();
    assert!(
        e.insert_plain_text_ctx(),
        "fresh engine is a plain insert-text context"
    );
    assert_eq!(
        e.insert_copy_char(Some('w')),
        Feed::Cmd(Command::InsertChar('w'))
    );
    assert_eq!(e.insert_copy_char(None), Feed::Ignored);
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
            case: WordCase::Downcase,
            wise: None,
        })
    );
    assert_eq!(
        feed("gU$"),
        Feed::Cmd(Command::CaseMotion {
            count: 1,
            motion: Motion::LineEnd,
            case: WordCase::Upcase,
            wise: None,
        })
    );
    assert_eq!(
        feed("g~w"),
        Feed::Cmd(Command::CaseMotion {
            count: 1,
            motion: Motion::WordFwd,
            case: WordCase::Toggle,
            wise: None,
        })
    );
    // Doubled → linewise.
    assert_eq!(
        feed("guu"),
        Feed::Cmd(Command::CaseMotion {
            count: 1,
            motion: Motion::Line,
            case: WordCase::Downcase,
            wise: None,
        })
    );
    assert_eq!(
        feed("gUU"),
        Feed::Cmd(Command::CaseMotion {
            count: 1,
            motion: Motion::Line,
            case: WordCase::Upcase,
            wise: None,
        })
    );
    assert_eq!(
        feed("g~~"),
        Feed::Cmd(Command::CaseMotion {
            count: 1,
            motion: Motion::Line,
            case: WordCase::Toggle,
            wise: None,
        })
    );
    // Count folds through: 2gUw.
    assert_eq!(
        feed("2gUw"),
        Feed::Cmd(Command::CaseMotion {
            count: 2,
            motion: Motion::WordFwd,
            case: WordCase::Upcase,
            wise: None,
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

#[test]
fn doubled_operator_is_linewise() {
    assert_eq!(feed("dd"), Feed::Cmd(Command::Delete(1, Motion::Line)));
    assert_eq!(feed("2dd"), Feed::Cmd(Command::Delete(2, Motion::Line)));
    assert_eq!(feed("cc"), Feed::Cmd(Command::Change(1, Motion::Line)));
}

#[test]
fn filter_operator_over_motion_emits_filtermotion() {
    // `!{motion}` is linewise-over-motion (like `=`/`gq`): the engine hands the motion + folded count to
    // the frontend, which numbers the lines and opens the `:{range}!` cmdline. No buffer edit here.
    assert_eq!(
        feed("!G"),
        Feed::FilterMotion {
            count: 1,
            motion: Motion::LastLine
        },
        "!G filters cursor→EOF"
    );
    assert_eq!(
        feed("!j"),
        Feed::FilterMotion {
            count: 1,
            motion: Motion::Down
        }
    );
    // The count folds operator × motion exactly as every other operator (`!2j` = 2 lines down).
    assert_eq!(
        feed("!2j"),
        Feed::FilterMotion {
            count: 2,
            motion: Motion::Down
        }
    );
    // Text objects compose (`!ip` filters the inner-paragraph lines).
    assert_eq!(
        feed("!ip"),
        Feed::FilterMotion {
            count: 1,
            motion: Motion::InnerParagraph
        }
    );
}

/// #800: `!` over a search, `n`, `*`/`#`, `gn` or a mark hands the span command to the frontend (which
/// numbers its lines) instead of aborting; the operator slot is the non-mutating `Yank` placeholder.
#[test]
fn filter_operator_over_span_commands_emits_filterspan() {
    let mut e = InputEngine::new();
    for c in "!/x".chars() {
        assert_eq!(e.feed(k(c), Mode::Normal), Feed::Pending);
    }
    assert_eq!(
        e.feed(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            Mode::Normal
        ),
        Feed::FilterSpan(Command::Search {
            op: SearchOp::Yank,
            count: 1,
            pattern: "x".into(),
            backward: false,
            offset: SearchOffset::None,
        })
    );
    // `n` after that search, with the filter's count folded in.
    let mut last = Feed::Ignored;
    for c in "2!n".chars() {
        last = e.feed(k(c), Mode::Normal);
    }
    assert_eq!(
        last,
        Feed::FilterSpan(Command::Search {
            op: SearchOp::Yank,
            count: 2,
            pattern: "x".into(),
            backward: false,
            offset: SearchOffset::None,
        })
    );
    for c in "!g".chars() {
        e.feed(k(c), Mode::Normal);
    }
    assert_eq!(
        e.feed(k('n'), Mode::Normal),
        Feed::FilterSpan(Command::SearchObject {
            op: SearchOp::Yank,
            count: 1,
            pattern: "x".into(),
            backward: false,
        })
    );
    assert_eq!(
        feed("!'a"),
        Feed::FilterSpan(Command::OpToMark {
            op: MarkOp::Yank,
            name: 'a',
            linewise: true,
        })
    );
    assert_eq!(
        feed("!#"),
        Feed::FilterSpan(Command::SearchWordUnder {
            forward: false,
            whole_word: true,
            op: SearchOp::Yank,
            count: 1,
        })
    );
    // The engine is back at a command boundary: the next key starts a new command.
    assert_eq!(e.pending(), Pending::Base);
}

/// #1071: `*`/`#`/`g*`/`g#` are operator motions (`d*`, `2c#`, `gUg*`), carrying operator and count.
#[test]
fn star_and_hash_continue_an_armed_operator() {
    assert_eq!(
        feed("2d3*"),
        Feed::Cmd(Command::SearchWordUnder {
            forward: true,
            whole_word: true,
            op: SearchOp::Delete,
            count: 6,
        })
    );
    assert_eq!(
        feed("c#"),
        Feed::Cmd(Command::SearchWordUnder {
            forward: false,
            whole_word: true,
            op: SearchOp::Change,
            count: 1,
        })
    );
    assert_eq!(
        feed("gUg*"),
        Feed::Cmd(Command::SearchWordUnder {
            forward: true,
            whole_word: false,
            op: SearchOp::Case(WordCase::Upcase),
            count: 1,
        })
    );
    assert_eq!(
        feed(">g#"),
        Feed::Cmd(Command::SearchWordUnder {
            forward: false,
            whole_word: false,
            op: SearchOp::Shift { left: false },
            count: 1,
        })
    );
}

#[test]
fn doubled_filter_operator_is_the_current_line_form() {
    // `!!` filters the current line; `{count}!!` filters `count` lines (like `>>`/`gqq`), linewise.
    assert_eq!(
        feed("!!"),
        Feed::FilterMotion {
            count: 1,
            motion: Motion::Line
        }
    );
    assert_eq!(
        feed("3!!"),
        Feed::FilterMotion {
            count: 3,
            motion: Motion::Line
        }
    );
}

#[test]
fn filter_cmdline_seed_then_command_runs_the_range_filter() {
    // The frontend seeds the `:{range}!` cmdline (Vim's `:.,.+N!`); typing the command + <CR> hands the
    // finished line to the SAME `:{range}!{cmd}` ex-filter path via `Feed::ExecuteEx`.
    let mut e = InputEngine::new();
    e.open_filter_cmdline("1,3!");
    // Pre-filled with the caret at its end (4 chars: `1`,`,`,`3`,`!`).
    assert_eq!(e.cmdline(), Some((':', "1,3!", 4)));
    for c in "sort".chars() {
        e.feed(k(c), Mode::Normal);
    }
    assert_eq!(
        e.feed(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            Mode::Normal
        ),
        Feed::ExecuteEx("1,3!sort".into())
    );
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
    assert_eq!(
        feed("]x"),
        Feed::Beep {
            clear_register: false
        }
    );
}

#[test]
fn bracket_keyword_lookup_emits_show_keyword_lines() {
    // `[i`/`]i` echo the count'th match (`[` from the top, `]` below the cursor); `[I`/`]I` list all.
    // A leading count carries through to the single (echo) forms.
    assert_eq!(
        feed("[i"),
        Feed::Cmd(Command::ShowKeywordLines {
            above: true,
            list: false,
            count: 1
        })
    );
    assert_eq!(
        feed("]i"),
        Feed::Cmd(Command::ShowKeywordLines {
            above: false,
            list: false,
            count: 1
        })
    );
    assert_eq!(
        feed("2[i"),
        Feed::Cmd(Command::ShowKeywordLines {
            above: true,
            list: false,
            count: 2
        })
    );
    assert_eq!(
        feed("[I"),
        Feed::Cmd(Command::ShowKeywordLines {
            above: true,
            list: true,
            count: 1
        })
    );
    assert_eq!(
        feed("]I"),
        Feed::Cmd(Command::ShowKeywordLines {
            above: false,
            list: true,
            count: 1
        })
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
            wise: None,
        })
    );
    // `g?ap` → ROT13 a paragraph; `g??` → the doubled linewise form.
    assert_eq!(
        feed("g?ap"),
        Feed::Cmd(Command::CaseMotion {
            count: 1,
            motion: Motion::AParagraph,
            case: WordCase::Rot13,
            wise: None,
        })
    );
    assert_eq!(
        feed("g??"),
        Feed::Cmd(Command::CaseMotion {
            count: 1,
            motion: Motion::Line,
            case: WordCase::Rot13,
            wise: None,
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
fn pending_reports_what_the_next_key_belongs_to() {
    // #679: the session's intercepts consult this before taking a key.
    let mut e = InputEngine::new();
    assert_eq!(e.pending(), Pending::Base);
    e.feed(k('3'), Mode::Normal);
    assert_eq!(e.pending(), Pending::Base, "a count is still a boundary");
    e.feed(k('d'), Mode::Normal);
    assert_eq!(e.pending(), Pending::Operator);
    e.feed(k('f'), Mode::Normal);
    assert_eq!(
        e.pending(),
        Pending::Argument,
        "the find char wins over the operator"
    );
    e.feed(k('z'), Mode::Normal);
    assert_eq!(e.pending(), Pending::Base);
    e.feed(k('['), Mode::Normal);
    assert_eq!(e.pending(), Pending::Bracket { open: true });
    e.take_count();
    for arg in ['r', 'm', '"', '\'', '`', 'g'] {
        e.feed(k(arg), Mode::Normal);
        assert_eq!(e.pending(), Pending::Argument, "after {arg:?}");
        e.take_count();
    }
    e.feed(k(':'), Mode::Normal);
    assert_eq!(
        e.pending(),
        Pending::Argument,
        "the command line owns its keys"
    );
}

#[test]
fn gqq_and_gqgq_format_count_lines() {
    let fmt = |count| {
        Feed::Cmd(Command::Format {
            count,
            motion: Motion::Line,
            keep_cursor: false,
        })
    };
    let mut e = InputEngine::new();
    for c in "2gq".chars() {
        e.feed(k(c), Mode::Normal);
    }
    assert_eq!(e.feed(k('q'), Mode::Normal), fmt(2));
    e.feed(k('g'), Mode::Normal);
    e.feed(k('q'), Mode::Normal);
    e.feed(k('g'), Mode::Normal);
    assert_eq!(e.feed(k('q'), Mode::Normal), fmt(1));
}

#[test]
fn semicolon_repeats_comma_reverses() {
    let mut e = InputEngine::new();
    e.feed(k('f'), Mode::Normal);
    e.feed(k('x'), Mode::Normal); // last_find = (x, forward, not-till)
    let again = |ch, forward, till| Motion::FindChar {
        ch,
        forward,
        till,
        repeat: true,
    };
    assert_eq!(
        e.feed(k(';'), Mode::Normal),
        Feed::Cmd(Command::Move(1, again('x', true, false))),
        "; repeats the last find"
    );
    assert_eq!(
        e.feed(k(','), Mode::Normal),
        Feed::Cmd(Command::Move(1, again('x', false, false))),
        ", repeats reversed"
    );
    // A repeated `t` is marked as a repeat, so the core skips the char it stopped before (#802).
    e.feed(k('t'), Mode::Normal);
    e.feed(k('x'), Mode::Normal);
    assert_eq!(
        e.feed(k(';'), Mode::Normal),
        Feed::Cmd(Command::Move(1, again('x', true, true)))
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
    // Bare `&` is the session's (it reads the count), never the engine's (#700).
    assert_ne!(feed("&"), Feed::Cmd(Command::RepeatSubstituteLine));
    assert_eq!(feed("`."), Feed::Cmd(Command::GotoLastChange));
    assert_eq!(feed("g;"), Feed::Cmd(Command::GotoOlderChange));
    assert_eq!(feed("g,"), Feed::Cmd(Command::GotoNewerChange));
    assert_eq!(feed("ma"), Feed::Cmd(Command::SetNamedMark('a')));
    assert_eq!(feed("`a"), Feed::Cmd(Command::GotoNamedMark('a')));
    assert_eq!(feed("gi"), Feed::Cmd(Command::InsertAtLastInsert));
    // `gI` — insert at column 0 (before indentation); `gr{char}` — classic virtual-replace one char.
    assert_eq!(feed("gI"), Feed::Cmd(Command::InsertColumnZero));
    assert_eq!(feed("grz"), Feed::Cmd(Command::VirtualReplaceChar(1, 'z')));
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
    // `3grz` — the count carries through `g` and the `gr` char-await into a count-3 virtual replace.
    let mut e = InputEngine::new();
    assert_eq!(e.feed(k('3'), Mode::Normal), Feed::Pending);
    assert_eq!(e.feed(k('g'), Mode::Normal), Feed::Pending);
    assert_eq!(e.feed(k('r'), Mode::Normal), Feed::Pending);
    assert_eq!(
        e.feed(k('z'), Mode::Normal),
        Feed::Cmd(Command::VirtualReplaceChar(3, 'z'))
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
fn info_commands_emit_frontend_resolved_actions() {
    let cg = || KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL);
    // `ga` — ascii info (the `:ascii`/`:as` ex synonym resolves to the same message downstream).
    assert_eq!(feed("ga"), Feed::Cmd(Command::AsciiInfo));
    // Plain `CTRL-G` in Normal is file info (previously an inert stub).
    let mut e = InputEngine::new();
    assert_eq!(e.feed(cg(), Mode::Normal), Feed::Cmd(Command::FileInfo));
    // `g CTRL-G` is cursor position/counts. It must NOT be mistaken for `gg` (which ignores the
    // modifier) nor consumed by the top-level `CTRL-G` handler (that only fires with no `g` pending).
    let mut e = InputEngine::new();
    assert_eq!(e.feed(k('g'), Mode::Normal), Feed::Pending);
    assert_eq!(e.feed(cg(), Mode::Normal), Feed::Cmd(Command::CursorInfo));
    // Regression guard: bare `gg` is still GotoLine, and `ga`'s `!ctrl` guard leaves the Visual
    // `g CTRL-A` sequence-increment intact.
    assert_eq!(feed("gg"), Feed::Cmd(Command::Move(1, Motion::GotoLine)));
    // `CTRL-G` in a selection still toggles Visual<->Select (unchanged behaviour).
    assert_eq!(
        e.feed(
            cg(),
            Mode::Visual {
                kind: SelectKind::Charwise
            }
        ),
        Feed::Cmd(Command::EnterSelect {
            kind: SelectKind::Charwise
        })
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

    // A second, DIFFERENT operator key aborts the armed one (nvim `checkclearop`, #790) — it does not
    // re-arm — so the following motion is a bare move.
    let mut e = InputEngine::new();
    assert_eq!(e.feed(k('>'), Mode::Normal), Feed::Pending);
    assert_eq!(
        e.feed(k('<'), Mode::Normal),
        Feed::Beep {
            clear_register: false
        }
    );
    assert_eq!(
        e.feed(k('j'), Mode::Normal),
        Feed::Cmd(Command::Move(1, Motion::Down))
    );
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
    assert_eq!(
        e.feed(k('z'), Mode::Normal),
        Feed::Beep {
            clear_register: false
        }
    );
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
