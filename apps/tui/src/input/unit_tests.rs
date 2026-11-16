//! The input engine's unit test suites, one child module per topic (#1398). The key-event
//! helpers every topic shares live here, so no topic depends on another.

use crate::input::*;

mod binding_label_tests;
mod cmdline_tests;
mod dot_repeat_tests;
mod ex_parse_tests;
mod insert_tests;
mod lang_arg_tests;
mod layer_state_tests;
mod namespace_tests;
mod normal_tests;
mod profile_tests;
mod search_tests;
mod state_machine_props_tests;
mod textobj_tests;
mod visual_tests;

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

pub(super) fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

pub(super) fn esc() -> KeyEvent {
    KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
}

pub(super) fn fc(ch: char, forward: bool, till: bool) -> Motion {
    Motion::FindChar {
        ch,
        forward,
        till,
        repeat: false,
    }
}
