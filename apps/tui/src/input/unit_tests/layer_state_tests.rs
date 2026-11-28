use super::{ctrl, k};
use crate::input::*;

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
/// command pops it, resuming Insert. `t_CTRL-\ CTRL-O` is the SAME construct (`resume: Terminal`,
/// below).
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

/// #1733 (KL-OBL-5): `t_CTRL-\ CTRL-O` records `resume: Terminal`. A completing command does NOT pop it
/// (the frontend ends it at a clean boundary, since a `:` line or a `v` selection spans completions),
/// and the Normal state survives between keys in Terminal-Normal while it is in flight (`2yk`, `"ayy`).
#[test]
fn terminal_one_shot_keeps_its_return_address_and_the_normal_state() {
    let mut e = InputEngine::new();
    e.begin_terminal_one_shot();
    assert_eq!(
        e.activations,
        vec![Suspended {
            resume: Ns::Terminal
        }]
    );
    assert_eq!(e.feed(k('2'), Mode::TerminalNormal), Feed::Pending);
    assert_eq!(e.normal.count, 2, "the count survives in Terminal-Normal");
    assert_eq!(e.feed(k('y'), Mode::TerminalNormal), Feed::Pending);
    assert!(!e.at_clean_boundary(), "the operator is still pending");
    assert!(matches!(e.feed(k('k'), Mode::TerminalNormal), Feed::Cmd(_)));
    assert!(e.at_clean_boundary());
    assert!(
        e.in_terminal_one_shot(),
        "completion leaves the Terminal return address for the frontend"
    );
    e.finish_terminal_one_shot();
    assert!(!e.in_terminal_one_shot());
    e.begin_terminal_one_shot();
    for c in ['"', 'a', 'y'] {
        let out = e.feed(k(c), Mode::TerminalNormal);
        assert!(!e.at_clean_boundary(), "`{c}` left {out:?} mid-command");
    }
    assert!(matches!(e.feed(k('y'), Mode::TerminalNormal), Feed::Cmd(_)));
    assert!(e.at_clean_boundary());
}

/// #1733: `CTRL-\ CTRL-N` inside a Terminal one-shot stays in Terminal-Normal (nvim `ntT` -> `nt`), so
/// it drops the return address and runs no command (it used to leave for plain Normal).
#[test]
fn ctrl_backslash_ctrl_n_ends_a_terminal_one_shot_in_terminal_normal() {
    let mut e = InputEngine::new();
    e.begin_terminal_one_shot();
    assert_eq!(e.feed(ctrl('\\'), Mode::TerminalNormal), Feed::Pending);
    assert_eq!(e.feed(ctrl('n'), Mode::TerminalNormal), Feed::Ignored);
    assert!(!e.in_terminal_one_shot());
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
