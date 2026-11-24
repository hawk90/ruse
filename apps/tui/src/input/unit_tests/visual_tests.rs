use super::{ctrl, esc, fc, feed, k};
use crate::input::*;

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
        Feed::Cmd(Command::PasteSelection {
            swap: true,
            count: 1,
        })
    );
    assert_eq!(
        e.feed(k('P'), vis),
        Feed::Cmd(Command::PasteSelection {
            swap: false,
            count: 1,
        })
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
    // In Visual, `o` emits SwapSelectionEnds (in Normal it is OpenBelow; in Select a typed `o` replaces
    // the selection, #792).
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
        Feed::Cmd(Command::ReplaceSelection('o'))
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
    // In Normal (no selection to toggle) CTRL-G is Vim's file-info command — NOT the start of `gg`.
    assert_eq!(e.feed(ctrl_g(), Mode::Normal), Feed::Cmd(Command::FileInfo));
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
fn select_printable_keys_replace_and_movement_extends() {
    // nvim v0.12.6 (`:help Select-mode`, #792): every printable key replaces the selection — the keys
    // Visual binds (`d`, `l`, `w`), the shared initiators (`f`, `g`, `"`, `m`, `` ` ``), digits and `:`.
    let sel = Mode::Select {
        kind: SelectKind::Charwise,
    };
    for c in [
        'd', 'y', 'c', 'l', 'w', 'f', 'g', 'G', '"', 'm', '`', '5', '0', ':', ' ', 'z', 'J',
    ] {
        let mut e = InputEngine::new();
        assert_eq!(
            e.feed(k(c), sel),
            Feed::Cmd(Command::ReplaceSelection(c)),
            "Select {c:?}"
        );
        assert!(e.normal.is_clean(), "Select {c:?} left grammar state");
    }
    let mut e = InputEngine::new();
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    // `<CR>` and `CTRL-J` replace with a line break; `<BS>`, `<Del>` and `CTRL-H` delete.
    let nl = Feed::Cmd(Command::ReplaceSelection('\n'));
    assert_eq!(e.feed(key(KeyCode::Enter), sel), nl);
    assert_eq!(e.feed(ctrl('j'), sel), nl);
    for k in [key(KeyCode::Backspace), key(KeyCode::Delete), ctrl('h')] {
        assert_eq!(e.feed(k, sel), Feed::Cmd(Command::DeleteSelection));
    }
    // Non-printable movement extends the selection (a bare Move the core re-plans against the anchor).
    assert_eq!(
        e.feed(key(KeyCode::Right), sel),
        Feed::Cmd(Command::Move(1, Motion::Right))
    );
    assert_eq!(
        e.feed(ctrl('n'), sel),
        Feed::Cmd(Command::Move(1, Motion::Down))
    );
    // `<Tab>` is not printable here: nothing happens (nvim beeps).
    assert_eq!(e.feed(key(KeyCode::Tab), sel), Feed::Ignored);
    // Esc leaves the selection.
    assert_eq!(e.feed(esc(), sel), Feed::Cmd(Command::EnterNormal));
}

#[test]
fn select_ctrl_o_runs_one_visual_command() {
    // `v_CTRL-O`: Visual for one command; one that keeps the selection returns to Select, one that
    // ends it (a change) runs plain so `.` records it.
    let sel = Mode::Select {
        kind: SelectKind::Charwise,
    };
    let vis = Mode::Visual {
        kind: SelectKind::Charwise,
    };
    let mut e = InputEngine::new();
    assert_eq!(
        e.feed(ctrl('o'), sel),
        Feed::Cmd(Command::EnterVisual {
            kind: SelectKind::Charwise
        })
    );
    assert_eq!(e.feed(k('2'), vis), Feed::Pending);
    assert_eq!(
        e.feed(k('e'), vis),
        Feed::Replay(vec![
            Command::Move(2, Motion::WordEnd),
            Command::EnterSelect {
                kind: SelectKind::Charwise
            }
        ])
    );
    // The shot is spent: the next Visual key is plain Visual.
    assert_eq!(
        e.feed(k('l'), vis),
        Feed::Cmd(Command::Move(1, Motion::Right))
    );
    // A change ends the selection and is not wrapped.
    e.feed(ctrl('o'), sel);
    assert_eq!(e.feed(k('d'), vis), Feed::Cmd(Command::DeleteSelection));
    // A shape switch returns to Select in the new shape.
    e.feed(ctrl('o'), sel);
    assert_eq!(
        e.feed(k('V'), vis),
        Feed::Replay(vec![
            Command::EnterVisual {
                kind: SelectKind::Linewise
            },
            Command::EnterSelect {
                kind: SelectKind::Linewise
            }
        ])
    );
}

#[test]
fn g_h_family_enters_select() {
    // `gh` / `gH` / `g CTRL-H` start Select charwise / linewise / blockwise; a count extends it.
    for (key, kind) in [
        (k('h'), SelectKind::Charwise),
        (k('H'), SelectKind::Linewise),
        (ctrl('h'), SelectKind::Blockwise),
    ] {
        let mut e = InputEngine::new();
        e.feed(k('g'), Mode::Normal);
        assert_eq!(
            e.feed(key, Mode::Normal),
            Feed::Replay(vec![
                Command::EnterVisual { kind },
                Command::EnterSelect { kind }
            ])
        );
    }
    let mut e = InputEngine::new();
    for c in "2g".chars() {
        e.feed(k(c), Mode::Normal);
    }
    assert_eq!(
        e.feed(k('H'), Mode::Normal),
        Feed::Replay(vec![
            Command::EnterVisual {
                kind: SelectKind::Linewise
            },
            Command::Move(1, Motion::Down),
            Command::EnterSelect {
                kind: SelectKind::Linewise
            }
        ])
    );
    // `gV` from Normal does nothing; `dgh` is not an operator form.
    let mut e = InputEngine::new();
    e.feed(k('g'), Mode::Normal);
    assert_eq!(e.feed(k('V'), Mode::Normal), Feed::Ignored);
    assert!(e.normal.is_clean());
}

#[test]
fn visual_line_operators_and_command_entry() {
    let vis = Mode::Visual {
        kind: SelectKind::Charwise,
    };
    let block = Mode::Visual {
        kind: SelectKind::Blockwise,
    };
    let op = |c| Feed::Cmd(Command::VisualOp(c));
    let one = |key: KeyEvent, mode: Mode| InputEngine::new().feed(key, mode);
    assert_eq!(one(k('D'), vis), op(VisualOp::Lines(OpKind::Delete)));
    assert_eq!(one(k('X'), vis), op(VisualOp::Lines(OpKind::Delete)));
    assert_eq!(one(k('Y'), vis), op(VisualOp::Lines(OpKind::Yank)));
    for c in ['C', 'S', 'R'] {
        assert_eq!(one(k(c), vis), op(VisualOp::Lines(OpKind::Change)));
    }
    assert_eq!(one(k('D'), block), op(VisualOp::BlockToEol(OpKind::Delete)));
    assert_eq!(one(k('C'), block), op(VisualOp::BlockToEol(OpKind::Change)));
    assert_eq!(one(k('X'), block), Feed::Cmd(Command::DeleteSelection));
    assert_eq!(one(k('Y'), block), Feed::Cmd(Command::YankSelection));
    assert_eq!(one(k('S'), block), op(VisualOp::Lines(OpKind::Change)));
    assert_eq!(one(k('O'), block), op(VisualOp::OtherCorner));
    assert_eq!(one(k('O'), vis), Feed::Cmd(Command::SwapSelectionEnds));
    assert_eq!(one(k('J'), vis), op(VisualOp::Join { spaces: true }));
    assert_eq!(one(k('='), vis), op(VisualOp::Reindent));
    assert_eq!(
        one(k('<'), vis),
        op(VisualOp::Shift {
            left: true,
            count: 1
        })
    );
    let mut e = InputEngine::new();
    e.feed(k('3'), vis);
    assert_eq!(
        e.feed(k('>'), vis),
        op(VisualOp::Shift {
            left: false,
            count: 3
        })
    );
    let mut e = InputEngine::new();
    e.feed(k('g'), vis);
    assert_eq!(e.feed(k('J'), vis), op(VisualOp::Join { spaces: false }));
    let mut e = InputEngine::new();
    e.feed(k('g'), vis);
    assert_eq!(e.feed(k('q'), vis), op(VisualOp::Format));
    // `:` / `!` leave Visual at its start and open the command line on the selection's range.
    let mut e = InputEngine::new();
    assert_eq!(e.feed(k(':'), vis), op(VisualOp::LeaveAtStart));
    assert_eq!(e.cmdline(), Some((':', "'<,'>", 5)));
    let mut e = InputEngine::new();
    assert_eq!(e.feed(k('!'), vis), op(VisualOp::LeaveAtStart));
    assert_eq!(e.cmdline(), Some((':', "'<,'>!", 6)));
}

#[test]
fn visual_count_gj_hands_off_then_clears_the_count() {
    // #1536: Visual `1gj` hands the screen motion to the frontend with the count still armed (the
    // frontend needs it); finishing it is the command boundary that clears it.
    let vis = Mode::Visual {
        kind: SelectKind::Charwise,
    };
    let mut e = InputEngine::new();
    assert_eq!(e.feed(k('1'), vis), Feed::Pending);
    assert_eq!(e.feed(k('g'), vis), Feed::Pending);
    assert_eq!(
        e.feed(k('j'), vis),
        Feed::ScreenMotion {
            motion: ScreenMotion::Down,
            count: 1
        }
    );
    assert_eq!(
        e.screen_motion_to(83),
        Feed::Cmd(Command::Move(84, Motion::GotoByte))
    );
    assert_eq!((e.normal.count, e.pending()), (0, Pending::Base));
    // The next count starts fresh: `2l`, not `12l`.
    assert_eq!(e.feed(k('2'), vis), Feed::Pending);
    assert_eq!(
        e.feed(k('l'), vis),
        Feed::Cmd(Command::Move(2, Motion::Right))
    );
}

#[test]
fn visual_star_hash_emit_search_selection() {
    // Visual `*`/`#` search for the selected text (nvim's `v_star-default`, #1557), carrying the count, in
    // every Visual kind; in Select a typed `*` replaces the selection instead.
    let mut e = InputEngine::new();
    for kind in [
        SelectKind::Charwise,
        SelectKind::Linewise,
        SelectKind::Blockwise,
    ] {
        let vis = Mode::Visual { kind };
        assert_eq!(
            e.feed(k('*'), vis),
            Feed::Cmd(Command::SearchSelection {
                forward: true,
                count: 1
            })
        );
        assert_eq!(e.feed(k('2'), vis), Feed::Pending);
        assert_eq!(
            e.feed(k('#'), vis),
            Feed::Cmd(Command::SearchSelection {
                forward: false,
                count: 2
            })
        );
    }
    let sel = Mode::Select {
        kind: SelectKind::Charwise,
    };
    assert_eq!(
        e.feed(k('*'), sel),
        Feed::Cmd(Command::ReplaceSelection('*'))
    );
}
