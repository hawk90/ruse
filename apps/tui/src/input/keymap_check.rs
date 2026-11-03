//! The static keymap conflict check for the key profiles (ENG-PROFILE-001 `static-conflict-check`, #1130).
//!
//! Walks every binding table the profiles declare and fails on a key bound twice in one table (the second
//! binding can never resolve) or a prefix key a higher layer hides behind a plain binding (every sequence
//! under it becomes unreachable):
//!
//! - the Vim namespaces' layers ([`VimProfile`], Insert and Replace carry bindings today);
//! - the Emacs nine-tier stack ([`EmacsProfile`]) and its `C-x` map ([`emacs::EMACS_CX_MAP`]);
//! - the Native leader menu ([`native::NATIVE_LEADER_MENU`]).
//!
//! No profile binds a multi-key sequence in a flat table yet, so there is no timeout rule to check: the
//! only sequences are a prefix key plus its sub-map, and the sub-map is its own table. The Vim Normal,
//! Visual and operator-pending keys are a grammar in code (count x operator x motion), not a table; the
//! modifier table in `chord_tests.rs` pins those per key against nvim instead.

use super::emacs::{EmacsBinding, EmacsProfile, EMACS_CX_MAP};
use super::native::NATIVE_LEADER_MENU;
use super::vim::{Ns, VimProfile};

/// The entries of `keys` that occur more than once, once each.
fn repeated<T: PartialEq + Copy>(keys: &[T]) -> Vec<T> {
    let mut out = Vec::new();
    for (i, k) in keys.iter().enumerate() {
        if keys[i + 1..].contains(k) && !out.contains(k) {
            out.push(*k);
        }
    }
    out
}

#[test]
fn vim_namespace_tables_have_no_conflicts() {
    let profile = VimProfile::new();
    let mut checked = 0;
    for ns in VimProfile::all().into_iter().chain([Ns::Replace]) {
        let stack = profile.stack(ns);
        checked += stack.layers().map(|l| l.len()).sum::<usize>();
        let found = stack.conflicts(|_| false);
        assert!(found.is_empty(), "{}: {found:?}", ns.id());
    }
    assert!(checked > 0, "the Vim profile declares bindings to check");
}

#[test]
fn emacs_tables_have_no_conflicts() {
    let profile = EmacsProfile::new();
    let found = profile
        .map
        .conflicts(|b| matches!(b, EmacsBinding::Prefix(_)));
    assert!(found.is_empty(), "{found:?}");
    let bound: usize = profile.map.layers().map(|l| l.len()).sum();
    assert!(bound > 30, "the global map is walked ({bound} bindings)");

    // Every prefix the stack binds has a sub-map, and the `C-x` map binds each key once.
    for l in profile.map.layers() {
        for (_, b) in l.bindings() {
            if let EmacsBinding::Prefix(p) = b {
                assert_eq!(*p, 'x', "prefix C-{p} has no sub-map to check");
            }
        }
    }
    let cx: Vec<(char, bool)> = EMACS_CX_MAP
        .iter()
        .map(|(c, ctrl, _)| (*c, *ctrl))
        .collect();
    assert_eq!(repeated(&cx), Vec::new(), "C-x map keys bound twice");
}

#[test]
fn native_leader_menu_has_no_conflicts() {
    let keys: Vec<char> = NATIVE_LEADER_MENU.iter().map(|(c, _, _)| *c).collect();
    assert_eq!(repeated(&keys), Vec::new(), "leader keys bound twice");
}

/// The checker itself catches what it claims to: a duplicate in a table and a shadowed prefix.
#[test]
fn the_check_catches_a_planted_conflict() {
    assert_eq!(
        repeated(&[('u', true), ('u', false), ('u', true)]),
        vec![('u', true)]
    );
    let mut profile = EmacsProfile::new();
    let mut extra = ruse_core::keymap::Layer::new(
        "emacs.keymaptier.06.minor-mode-map-alist.test",
        450,
        false,
        ruse_core::keymap::UnmatchedKey::Ignore,
    );
    let cx = super::emacs::EmacsKey::of(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('x'),
        crossterm::event::KeyModifiers::CONTROL,
    ));
    extra = extra
        .bind(cx, EmacsBinding::Fixed(ruse_core::Command::Undo))
        .bind(cx, EmacsBinding::Fixed(ruse_core::Command::Redo));
    profile.map.push(extra).expect("a fresh rank and id");
    let found = profile
        .map
        .conflicts(|b| matches!(b, EmacsBinding::Prefix(_)));
    assert_eq!(found.len(), 2, "{found:?}");
}
