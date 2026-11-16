//! A namespace's declared unmatched-key policy (D-045): what a key nothing bound does, decided by the
//! layer that was consulted. Moved out of `input/mod.rs` as an `impl InputEngine` block (#1493).

use crossterm::event::{KeyCode, KeyEvent};
use ruse_core::keymap::{Resolved, UnmatchedKey};
use ruse_core::Command;

use super::{is_chord, Feed, InputEngine, Ns};

impl InputEngine {
    /// Apply a namespace's declared unmatched-key policy to a key nothing bound.
    ///
    /// This is the replacement for the shared `Feed::Ignored` fallthrough. The behaviour is
    /// deliberately unchanged today — `Ignore` and `Abort` both clear the transient state and yield
    /// `Ignored` — but the DECISION now comes from the layer that was actually consulted instead of
    /// from one arm at the bottom of `feed`. Separating them properly needs KL-OBL-4 (a layer owns
    /// its state and dies with it), which this engine does not model yet: count/operator/awaiting are
    /// still engine-wide, so `reset()` is the only available approximation of "the layer went away".
    pub(super) fn unmatched(&mut self, ns: Ns, key: KeyEvent) -> Feed {
        let had_register = self.normal.register;
        let feed = self.unmatched_policy(ns, key);
        self.drop_aborted_register(had_register, feed)
    }

    /// An abort also abandons a `"x` prefix already handed to the core (`"b<Esc>p` pastes the UNNAMED
    /// register in nvim): clear the core's pending register in place of the bare `Ignored`.
    pub(super) fn drop_aborted_register(&self, had_register: bool, feed: Feed) -> Feed {
        match feed {
            Feed::Ignored if had_register => Feed::Cmd(Command::SetRegister(None)),
            Feed::Beep { .. } => Feed::Beep {
                clear_register: had_register,
            },
            feed => feed,
        }
    }

    pub(super) fn unmatched_policy(&mut self, ns: Ns, key: KeyEvent) -> Feed {
        let policy = match self.profile.stack(ns).resolve(&key.code) {
            Resolved::Bound { .. } => {
                // Reachable only if a caller routes a bound key here; treat as unhandled rather than
                // guessing, so a wiring mistake shows up instead of silently doing something.
                self.reset();
                return Feed::Ignored;
            }
            Resolved::Unmatched { policy, .. } => policy,
            // An empty stack is a construction bug (see `VimProfile::new`), never a policy.
            Resolved::NoLayer => unreachable!("every Vim namespace declares exactly one layer"),
        };
        match policy {
            UnmatchedKey::Insert => {
                self.reset();
                match key.code {
                    // An unbound chord inserts nothing, never its letter (#599).
                    KeyCode::Char(c) if !is_chord(key) => Feed::Cmd(Command::InsertChar(c)),
                    // `open/insert` is about PRINTABLE keys; a non-printable unmatched key does
                    // nothing, which is not the same statement as `closed/ignore`.
                    _ => Feed::Ignored,
                }
            }
            UnmatchedKey::Ignore | UnmatchedKey::Abort => {
                self.reset();
                // An unknown key in Normal, Visual or after an operator beeps in Vim (`nv_error` →
                // `clearopbeep`), which flushes the keys not typed. `<Esc>` beeps without flushing
                // (`nv_esc`), and a bare modifier or lock key is no key at all (#1111). An unbound chord
                // stays a plain no-op: several are nvim commands ruse resolves elsewhere or lacks (Visual
                // `<C-d>` scrolls in nvim), and failing them would stop a macro nvim runs on.
                let no_key = is_chord(key)
                    || matches!(
                        key.code,
                        KeyCode::Esc
                            | KeyCode::Modifier(_)
                            | KeyCode::Null
                            | KeyCode::CapsLock
                            | KeyCode::NumLock
                            | KeyCode::ScrollLock
                            | KeyCode::Media(_)
                    );
                if no_key {
                    Feed::Ignored
                } else {
                    Feed::Beep {
                        clear_register: false,
                    }
                }
            }
            UnmatchedKey::ReplaceSelection => {
                self.reset();
                match key.code {
                    // Vim Select: a printable key deletes the selection, inserts the char, enters Insert.
                    // The core (`Command::ReplaceSelection`) performs all three as one edit.
                    KeyCode::Char(c) if !is_chord(key) => Feed::Cmd(Command::ReplaceSelection(c)),
                    // `open/replace-selection` is about PRINTABLE keys; a non-printable unmatched key
                    // does nothing (it is NOT `closed/ignore`, but the observable result here matches).
                    _ => Feed::Ignored,
                }
            }
            // The remaining open policies belong to namespaces this engine does not reach yet
            // (Cmdline/Terminal/Lang). Reaching one means a namespace was wired without its handler,
            // and failing loudly beats inventing a behaviour.
            other => unreachable!("namespace {ns:?} has unimplemented policy {other:?}"),
        }
    }
}
