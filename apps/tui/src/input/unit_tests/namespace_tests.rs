use super::k;
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
