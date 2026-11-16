//! The palette's binding column (F-004 #2, #956): [`InputEngine::binding_labels`] finds the keys that
//! run a command by feeding probe keys to a fresh engine. Moved out of `input/mod.rs` (#1493).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ruse_core::{Command, Mode, Motion};

use super::{EmacsAction, Feed, InputEngine};

/// A short human label for a key, for the palette's binding column (F-004 #2).
fn key_label(code: KeyCode) -> String {
    match code {
        KeyCode::Esc => "Esc".into(),
        KeyCode::Enter => "CR".into(),
        KeyCode::Backspace => "BS".into(),
        KeyCode::Tab => "Tab".into(),
        KeyCode::Char(' ') => "Space".into(),
        KeyCode::Char(c) => c.to_string(),
        other => format!("{other:?}"),
    }
}

/// One key as the palette shows it: `u`, `J`, `C-r`, `M-x`, `Esc`.
fn probe_label(key: KeyEvent) -> String {
    let base = key_label(key.code);
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        format!("C-{base}")
    } else if key.modifiers.contains(KeyModifiers::ALT) {
        format!("M-{base}")
    } else {
        base
    }
}

/// The keys [`InputEngine::binding_labels`] tries, simplest first: plain printable keys (an uppercase
/// letter with SHIFT, as a terminal reports it), the special keys, CTRL chords, then Meta chords.
fn probe_keys() -> Vec<KeyEvent> {
    let mut keys = Vec::new();
    for c in ' '..='~' {
        let m = if c.is_ascii_uppercase() {
            KeyModifiers::SHIFT
        } else {
            KeyModifiers::NONE
        };
        keys.push(KeyEvent::new(KeyCode::Char(c), m));
    }
    for code in [
        KeyCode::Esc,
        KeyCode::Enter,
        KeyCode::Backspace,
        KeyCode::Tab,
        KeyCode::Delete,
        KeyCode::Insert,
        KeyCode::Home,
        KeyCode::End,
        KeyCode::PageUp,
        KeyCode::PageDown,
        KeyCode::Left,
        KeyCode::Right,
        KeyCode::Up,
        KeyCode::Down,
    ] {
        keys.push(KeyEvent::new(code, KeyModifiers::NONE));
    }
    for c in ('a'..='z').chain(['/', '_', '@', ' ', ']', '\\', '^']) {
        keys.push(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
    }
    for c in 'a'..='z' {
        keys.push(KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT));
    }
    keys
}

/// Label every still-unlabelled entry of `commands` that `feed` (the result of typing `seq`) runs.
fn note_binding(out: &mut [Option<String>], commands: &[Command], feed: &Feed, seq: &[KeyEvent]) {
    let cmd = match feed {
        Feed::Cmd(c) => c,
        Feed::Replay(cs) if cs.len() == 1 => &cs[0],
        // Emacs `C-x C-c` is the palette's Quit, which the session runs as `save-buffers-kill-terminal`.
        Feed::EmacsAction(EmacsAction::KillTerminal) => &Command::Quit,
        _ => return,
    };
    // `0` and `$` are the counted motions; the registry names the same moves without a count.
    let same = |want: &Command| {
        want == cmd
            || matches!(
                (want, cmd),
                (Command::MoveLineStart, Command::Move(1, Motion::LineStart))
                    | (Command::MoveLineEnd, Command::Move(1, Motion::LineEnd))
            )
    };
    for (slot, want) in out.iter_mut().zip(commands) {
        if slot.is_none() && same(want) {
            let labels: Vec<String> = seq.iter().map(|&k| probe_label(k)).collect();
            // `gJ`, `ZZ` read as one word; a chord in the sequence is spaced (`C-x C-s`, `C-g u`).
            *slot = Some(if labels.iter().all(|l| l.chars().count() == 1) {
                labels.concat()
            } else {
                labels.join(" ")
            });
        }
    }
}

impl InputEngine {
    /// Human labels for the keys that run each of `commands` in `mode` under this engine's profile (F-004
    /// #2 palette column, #956), `None` where no key of one or two strokes does. Found by feeding keys to a
    /// fresh engine of the same profile, so the label is what typing it does, grammar keys included (`u`,
    /// `J`, `0`, `CTRL-R`, Emacs `C-x C-s`), and not a table that can drift from the grammar. One-key
    /// bindings win over two-key ones; a count is never part of a binding.
    #[must_use]
    pub fn binding_labels(&self, commands: &[Command], mode: Mode) -> Vec<Option<String>> {
        let mut out: Vec<Option<String>> = vec![None; commands.len()];
        let keys = probe_keys();
        let profile = self.input_profile;
        let feed_seq = |seq: &[KeyEvent]| {
            let mut e = InputEngine::with_profile(profile);
            let mut last = Feed::Pending;
            for &k in seq {
                last = e.feed(k, mode);
            }
            last
        };
        let mut prefixes = Vec::new();
        for &k in &keys {
            let feed = feed_seq(&[k]);
            // A count digit is pending too, but a count is not part of a binding.
            let count = k.modifiers.is_empty() && matches!(k.code, KeyCode::Char('1'..='9'));
            if matches!(feed, Feed::Pending) && !count {
                prefixes.push(k);
            }
            note_binding(&mut out, commands, &feed, &[k]);
        }
        for &p in &prefixes {
            if out.iter().all(Option::is_some) {
                break;
            }
            for &k in &keys {
                let feed = feed_seq(&[p, k]);
                note_binding(&mut out, commands, &feed, &[p, k]);
            }
        }
        out
    }
}
