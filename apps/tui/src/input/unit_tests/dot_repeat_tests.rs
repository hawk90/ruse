//! Dot-repeat (`.`): the engine records the last change as a re-parameterizable [`ChangeIntent`] and
//! replays it — the operator/edit at the CURRENT cursor, plus any captured insert text (F-023).

use super::{ctrl, esc, k};
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
            | Feed::Cmd(Command::InsertColumnZero)
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
    assert_eq!(
        e.feed(k('.'), Mode::Normal),
        Feed::Beep {
            clear_register: false
        }
    );
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
fn a_failed_change_puts_the_earlier_one_back() {
    // `x`, then `dfz` fails in the core (no `z`): the engine forgets it, so `.` repeats the `x` (#1179).
    let mut e = InputEngine::new();
    assert_eq!(
        e.feed(k('x'), Mode::Normal),
        Feed::Cmd(Command::DeleteUnder(1))
    );
    for key in [k('d'), k('f')] {
        e.feed(key, Mode::Normal);
    }
    let Feed::Cmd(dfz) = e.feed(k('z'), Mode::Normal) else {
        panic!("dfz is one command");
    };
    e.change_failed(&dfz, false);
    assert_eq!(
        e.feed(k('.'), Mode::Normal),
        Feed::Replay(vec![Command::DeleteUnder(1)])
    );
    // Only the command just recorded is forgotten: a second report changes nothing.
    e.change_failed(&dfz, false);
    assert_eq!(
        e.feed(k('.'), Mode::Normal),
        Feed::Replay(vec![Command::DeleteUnder(1)])
    );
}

#[test]
fn a_change_nvim_saves_before_it_fails_stays() {
    // `CTRL-A` without a number fails after nvim's `prep_redo`, and a refused `"/dw` inside its operator.
    let mut e = InputEngine::new();
    e.feed(k('x'), Mode::Normal);
    let Feed::Cmd(add) = e.feed(ctrl('a'), Mode::Normal) else {
        panic!("CTRL-A is one command");
    };
    e.change_failed(&add, false);
    assert_eq!(e.feed(k('.'), Mode::Normal), Feed::Replay(vec![add]));
    for key in [k('"'), k('/'), k('d')] {
        e.feed(key, Mode::Normal);
    }
    let Feed::Cmd(dw) = e.feed(k('w'), Mode::Normal) else {
        panic!("dw is one command");
    };
    e.change_failed(&dw, true);
    assert_eq!(
        e.feed(k('.'), Mode::Normal),
        Feed::Replay(vec![Command::SetRegister(Some('/')), dw])
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
    assert_eq!(
        e.feed(k('.'), Mode::Normal),
        Feed::Beep {
            clear_register: false
        }
    );
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
