//! The Native profile's static grammar (F-013): the leader / which-key menu (NAT-2). Split out of
//! `input/mod.rs` as self-contained profile DATA — the engine methods that consult it (`feed`'s leader
//! dispatch, `leader_hint`) stay in the engine core, a deliberate partial split like [`super::vim`].

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ruse_core::Command;

/// The Native profile's leader (which-key) map (F-013 NAT-2) — the SEED of `native-profile@1`'s recommended
/// keymap. `<leader>` (Space) from a clean Normal base opens it; the next key resolves HERE to a semantic
/// command (INV-CMD-SEMANTIC) or aborts. It binds only commands that ALREADY exist — the Files/Git/Debug
/// discovery groups from the design (`docs/parity/native-style.md`) land as those features do, not before.
/// Intentionally-different from Vim/Emacs: a new discovery grammar, not a blend (NAT-2, D-051 spirit).
pub(crate) const NATIVE_LEADER_MENU: &[(char, &str, Command)] = &[
    ('w', "write", Command::Save),
    ('q', "quit", Command::Quit),
    ('u', "undo", Command::Undo),
    ('r', "redo", Command::Redo),
];

/// Resolve a leader selection key to its bound command, or `None` if the key is unbound — a which-key abort
/// (Emacs `C-g` / any key not on the menu closes it). Only an unmodified (or Shift-only) char key can bind.
pub(crate) fn native_leader_command(key: KeyEvent) -> Option<Command> {
    let KeyCode::Char(c) = key.code else {
        return None;
    };
    if !(key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT) {
        return None;
    }
    NATIVE_LEADER_MENU
        .iter()
        .find(|(k, _, _)| *k == c)
        .map(|(_, _, cmd)| cmd.clone())
}
