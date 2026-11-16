//! Property tests over the input state machine: the hierarchy is explicit, so verify it has no holes —
//! no key sequence leaks a partial command, `awaiting` and the operator axis stay consistent, and `feed`
//! is deterministic. This is the mechanical guard against the "ad-hoc resolution order" class of bug.

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
    #![proptest_config(crate::test_cases(256))]

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
