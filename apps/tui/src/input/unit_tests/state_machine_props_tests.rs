//! Property tests over the input state machine: the hierarchy is explicit, so verify it has no holes —
//! no key sequence leaks a partial command, `awaiting` and the operator axis stay consistent, and `feed`
//! is deterministic. This is the mechanical guard against the "ad-hoc resolution order" class of bug.

use crate::input::*;
use proptest::prelude::*;

/// A key drawn from the meaningful command alphabet, plus arbitrary chars (find targets) and specials.
fn any_key() -> impl Strategy<Value = KeyEvent> {
    let named = "0123456789hjklwbeWBEdcyiaoOAIxfFtT;,vVpPunN$/:gGrJ~%(){}[]<>\"'`spHML."
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

/// The viewport motion `key` is when the frontend takes it before the engine, as `screen_line_owns`
/// does for `H` / `M` / `L` in Normal mode at a command boundary or under an operator (#1577). The
/// property draws a mode per key, so it also asks for no Insert-side prefix and no command line: in the
/// session the view cannot be in Normal mode with one of them open.
fn viewport_key(e: &InputEngine, key: KeyEvent, mode: Mode) -> Option<ScreenMotion> {
    let insert_state = e.insert.ctrl_g || e.insert.ctrl_r || e.insert.ctrl_x || e.insert.keep_undo;
    if mode != Mode::Normal
        || !key.modifiers.is_empty()
        || insert_state
        || !e.activations.is_empty()
        || e.cmdline.is_some()
        || !matches!(e.pending(), Pending::Base | Pending::Operator)
    {
        return None;
    }
    match key.code {
        KeyCode::Char('H') => Some(ScreenMotion::ViewHigh),
        KeyCode::Char('M') => Some(ScreenMotion::ViewMiddle),
        KeyCode::Char('L') => Some(ScreenMotion::ViewLow),
        _ => None,
    }
}

/// Finish a `Feed::ScreenMotion` the way the frontend's `resolve_screen_motion` can, rotating through
/// its four finishers by step index so every one of them is held to the no-leak invariant. A viewport
/// motion (`.` of a `dL`) is finished as the session does, through `screen_op`.
fn finish_screen_motion(
    e: &mut InputEngine,
    motion: ScreenMotion,
    count: u32,
    step: usize,
) -> Feed {
    if motion.is_viewport() {
        return e.screen_op(motion, count, 1 + (step % 3) as u32);
    }
    match step % 4 {
        0 => e.screen_motion_to(0),
        1 => e.screen_motion_line_end(1),
        2 => e.screen_motion_lines(step % 8 < 4),
        _ => e.screen_motion_fail(),
    }
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
    #![proptest_config(crate::test_cases(256))]

    /// No key sequence ever leaves a partial command dangling: after any outcome that is not
    /// `Feed::Pending`, every transient axis (count, operator, key-expectation) is cleared. And a
    /// text-object expectation is only ever armed with an operator present. (Never panics — implicit.)
    ///
    /// A `Feed::ScreenMotion` (`gj`, `gk`, …) is a handoff, not an outcome: the engine keeps the count,
    /// operator, register and any `i_CTRL-O` one-shot armed until the frontend finishes it (#615, #1177).
    /// The property finishes it as the frontend does and checks the finishing outcome instead (#1536).
    #[test]
    fn no_pending_state_ever_leaks(steps in prop::collection::vec((any_key(), any_mode()), 0..80)) {
        let mut e = InputEngine::new();
        for (i, (key, mode)) in steps.into_iter().enumerate() {
            // `H` / `M` / `L` never reach `feed`: the frontend resolves the line and finishes them (#1577).
            let mut feed = match viewport_key(&e, key, mode) {
                Some(motion) => {
                    let count = e.screen_count();
                    e.screen_op(motion, count, 1 + (i % 3) as u32)
                }
                None => e.feed(key, mode),
            };
            if let Feed::ScreenMotion { motion, count } = feed {
                // `.` of a change over a screen motion hands it out with the recorded count and no
                // operator armed, though it resolves as one (#1539).
                let redo = e.has_op() && e.normal.op.is_none();
                if !redo {
                    prop_assert_eq!(count, e.mcount(), "handoff count differs from the armed count");
                }
                prop_assert!(e.normal.awaiting == Awaiting::Nothing, "key-expectation armed across a handoff");
                feed = finish_screen_motion(&mut e, motion, count, i);
            }
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
                    || e.insert.ctrl_x // the `i_CTRL-X` submode awaits its selector (#602)
                    || e.insert.keep_undo // `CTRL-G U` arms the next `<Left>`/`<Right>` (#1076)
                    || e.cmdline.is_some(); // an open command-line namespace is real pending state (F-026)
                prop_assert!(has_state, "Feed::Pending but the engine is idle");
            } else {
                prop_assert_eq!(e.normal.count, 0, "count leaked after {:?}", feed);
                prop_assert!(e.normal.op.is_none(), "operator leaked after {:?}", feed);
                prop_assert!(e.normal.awaiting == Awaiting::Nothing, "key-expectation leaked after {:?}", feed);
                prop_assert!(e.activations.is_empty(), "one-shot leaked after {:?}", feed);
                prop_assert!(!e.insert.ctrl_g, "ctrl-g prefix leaked after {:?}", feed);
                prop_assert!(!e.insert.ctrl_r, "ctrl-r prefix leaked after {:?}", feed);
                prop_assert!(!e.insert.ctrl_x, "ctrl-x submode leaked after {:?}", feed);
                prop_assert!(e.screen_handoff.is_none(), "screen-motion handoff leaked after {:?}", feed);
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
