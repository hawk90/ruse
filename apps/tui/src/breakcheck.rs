//! The terminal side of the user interrupt (nvim's `os_breakcheck`; core: [`ruse_core::interrupt`]).
//!
//! Raw mode turns CTRL-C into a key, so an interrupt can only be seen by reading the terminal. While a long
//! command runs (a macro replay, a counted repeat, a `:g`, a `:s`), the core's checkpoint calls [`poll`]
//! about every [`ruse_core::interrupt::POLL_INTERVAL`]: it drains the typeahead without blocking, and the
//! interrupt key (CTRL-C; `C-g` in the Emacs profile, Emacs's `keyboard-quit`) raises the interrupt. Input
//! read on the way is kept, in order, for the event loop ([`pop`]), except what came before an interrupt
//! key: that is flushed, and the key itself is kept and read next, as nvim's `process_ctrl_c` does. A
//! fatal signal (#857) counts as an interrupt too, so a long command unwinds and the loop can end the
//! session.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// At most this many events are kept while a command runs; more typeahead than this is dropped.
const MAX_TYPEAHEAD: usize = 4096;

thread_local! {
    static TYPEAHEAD: RefCell<VecDeque<Event>> = const { RefCell::new(VecDeque::new()) };
    static EMACS: Cell<bool> = const { Cell::new(false) };
}

/// Install [`poll`] as this thread's interrupt hook. `emacs` picks `C-g` instead of CTRL-C as the key.
pub(crate) fn install(emacs: bool) {
    EMACS.with(|e| e.set(emacs));
    ruse_core::interrupt::set_hook(Some(poll));
}

/// Whether `key` interrupts a running command in the profile [`install`]ed.
pub(crate) fn is_interrupt_key(key: KeyEvent, emacs: bool) -> bool {
    let c = if emacs { 'g' } else { 'c' };
    key.kind != KeyEventKind::Release
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char(k) if k.eq_ignore_ascii_case(&c))
}

/// The hook: read every pending event without blocking, and report whether the interrupt key came.
fn poll() -> bool {
    if crate::signals::pending().is_some() {
        return true;
    }
    let emacs = EMACS.with(Cell::get);
    let mut interrupted = false;
    while matches!(event::poll(Duration::ZERO), Ok(true)) {
        let Ok(ev) = event::read() else { break };
        if matches!(ev, Event::Key(k) if is_interrupt_key(k, emacs)) {
            // nvim's `process_ctrl_c`: the typeahead before the CTRL-C goes, the CTRL-C itself stays and
            // is then read as a key (so an interrupted Insert ends as `i_CTRL-C` ends it).
            interrupted = true;
            TYPEAHEAD.with(|t| t.borrow_mut().clear());
        }
        TYPEAHEAD.with(|t| {
            let mut t = t.borrow_mut();
            if t.len() < MAX_TYPEAHEAD {
                t.push_back(ev);
            }
        });
    }
    interrupted
}

/// The next event [`poll`] read ahead while a command ran, if any. The event loop takes these before it
/// reads the terminal again.
pub(crate) fn pop() -> Option<Event> {
    TYPEAHEAD.with(|t| t.borrow_mut().pop_front())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_interrupt_key_follows_the_profile() {
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        assert!(is_interrupt_key(ctrl('c'), false));
        assert!(!is_interrupt_key(ctrl('g'), false));
        assert!(is_interrupt_key(ctrl('g'), true));
        assert!(
            !is_interrupt_key(ctrl('c'), true),
            "C-c is a prefix in Emacs"
        );
        assert!(!is_interrupt_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
            false
        ));
    }
}
