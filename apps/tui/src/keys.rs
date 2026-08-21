//! The macro key codec (D-055): a round-trippable `KeyEvent` ↔ bytes encoding. A recorded Vim macro is the
//! RAW keystroke stream stored as bytes in a register, so `encode` mirrors what a terminal delivers (and what
//! `pty::encode_key` forwards — but that one is lossy/one-way) and `decode` inverts it. The slice-1 alphabet
//! is printable UTF-8 + `Esc`/`Enter`/`Tab`/`Backspace` + `Ctrl-a..z`; other keys are DEFERRED (they encode to
//! nothing, and `decode` skips bytes it does not recognise, so a hand-edited register still runs).

use std::collections::VecDeque;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// The recursion guard for macro replay (D-055): the maximum keys ONE top-level `@` may expand to, across
/// any nested `@` it triggers. A self-invoking macro hits this and stops rather than hanging.
pub const MACRO_KEY_CAP: u32 = 100_000;

/// Enqueue a replayed macro's decoded keys onto `queue`, honouring `budget` — the remaining key allowance
/// shared across a whole top-level `@` expansion (nested `@`s draw from the same budget, so recursion
/// terminates). A top-level `@` (empty queue, spent budget) starts a fresh [`MACRO_KEY_CAP`]. Returns
/// `false` if the macro was TRUNCATED at the cap (the caller shows a status), `true` if fully enqueued.
pub fn enqueue_macro(queue: &mut VecDeque<KeyEvent>, budget: &mut u32, bytes: &[u8]) -> bool {
    let mut keys = decode(bytes);
    if queue.is_empty() && *budget == 0 {
        *budget = MACRO_KEY_CAP; // top-level `@`: a fresh budget for its whole expansion
    }
    let take = keys.len().min(*budget as usize);
    let full = take == keys.len();
    keys.truncate(take);
    *budget -= take as u32;
    queue.extend(keys);
    full
}

/// Encode one key event to its macro bytes, or `None` for a key outside the slice-1 alphabet (arrows, Fn,
/// Alt, …) — the recorder simply drops those for now (D-055 defers them). Printable chars encode as UTF-8;
/// `Ctrl`-`a`..`z` collapse to `0x01`..`0x1a`, matching the control bytes a terminal actually sends.
pub fn encode(key: KeyEvent) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let utf8 = |c: char| {
        let mut b = [0u8; 4];
        c.encode_utf8(&mut b).as_bytes().to_vec()
    };
    let bytes = match key.code {
        KeyCode::Char(c) if ctrl && c.to_ascii_lowercase().is_ascii_alphabetic() => {
            vec![c.to_ascii_lowercase() as u8 - b'a' + 1] // C-a=0x01 … C-z=0x1a
        }
        KeyCode::Char(c) => utf8(c),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        _ => return None, // arrows / Fn / Alt / … — deferred (PR C)
    };
    Some(bytes)
}

/// Encode a whole key sequence to bytes (concatenation of [`encode`], skipping unencodable keys).
pub fn encode_all(keys: &[KeyEvent]) -> Vec<u8> {
    let mut out = Vec::new();
    for &k in keys {
        if let Some(b) = encode(k) {
            out.extend(b);
        }
    }
    out
}

/// Decode macro bytes back to key events. TOLERANT: an unrecognised or malformed byte is skipped (never a
/// panic), so a hand-edited or partially-yanked register still replays as far as it can. The specific
/// control bytes (`Esc`/`CR`/`Tab`/`BS`) take priority over the generic `Ctrl`-letter rule, matching how
/// [`encode`] produced them; the rest of `0x01`..`0x1a` become `Ctrl`-letter.
pub fn decode(bytes: &[u8]) -> Vec<KeyEvent> {
    let plain = |code| KeyEvent::new(code, KeyModifiers::NONE);
    let ctrl = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            0x1b => out.push(plain(KeyCode::Esc)),
            b'\r' | b'\n' => out.push(plain(KeyCode::Enter)),
            b'\t' => out.push(plain(KeyCode::Tab)),
            0x7f | 0x08 => out.push(plain(KeyCode::Backspace)),
            0x01..=0x1a => out.push(ctrl((b - 1 + b'a') as char)), // C-a..C-z (Tab/CR handled above)
            0x00..=0x1f => {} // other C0 controls not in the alphabet — skip
            _ => {
                // A UTF-8 scalar: consume its full width (1–4 bytes); a malformed lead byte is skipped.
                let width = utf8_width(b);
                if let Ok(s) = std::str::from_utf8(&bytes[i..(i + width).min(bytes.len())]) {
                    if let Some(c) = s.chars().next() {
                        out.push(plain(KeyCode::Char(c)));
                        i += c.len_utf8();
                        continue;
                    }
                }
                // not valid UTF-8 here — skip this byte
            }
        }
        i += 1;
    }
    out
}

/// The byte-width of a UTF-8 sequence from its lead byte (1 for ASCII / invalid lead, so the caller advances).
fn utf8_width(lead: u8) -> usize {
    match lead {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn round_trips_the_slice1_alphabet() {
        let keys = vec![
            ch('a'),
            ch('Z'),
            ch('가'), // multibyte UTF-8
            ch(' '),
            ctrl('a'),
            ctrl('w'),
            key(KeyCode::Esc),
            key(KeyCode::Enter),
            key(KeyCode::Tab),
            key(KeyCode::Backspace),
        ];
        for k in &keys {
            assert_eq!(decode(&encode(*k).unwrap()), vec![*k], "round-trip {k:?}");
        }
    }

    #[test]
    fn a_whole_macro_buffer_round_trips() {
        // "iZ<Esc>" — enter insert, type Z, escape.
        let macro_keys = vec![ch('i'), ch('Z'), key(KeyCode::Esc)];
        let bytes = encode_all(&macro_keys);
        assert_eq!(bytes, vec![b'i', b'Z', 0x1b]);
        assert_eq!(decode(&bytes), macro_keys);
    }

    #[test]
    fn ctrl_letters_map_to_control_bytes() {
        assert_eq!(encode(ctrl('a')).unwrap(), vec![0x01]);
        assert_eq!(encode(ctrl('z')).unwrap(), vec![0x1a]);
        // Ctrl-I / Ctrl-M are the Tab / Enter bytes, so they decode to Tab / Enter (terminal ambiguity).
        assert_eq!(decode(&[0x09]), vec![key(KeyCode::Tab)]);
        assert_eq!(decode(&[0x0d]), vec![key(KeyCode::Enter)]);
    }

    #[test]
    fn decode_skips_unrecognised_bytes_without_panic() {
        // A stray 0x00 and an incomplete UTF-8 lead byte are skipped; the surrounding keys survive.
        let bytes = vec![b'x', 0x00, 0xff, b'y'];
        assert_eq!(decode(&bytes), vec![ch('x'), ch('y')]);
    }

    #[test]
    fn arrows_and_unencodable_keys_are_dropped() {
        assert_eq!(encode(key(KeyCode::Up)), None);
        // encode_all silently drops them, keeping the rest of the sequence.
        assert_eq!(
            encode_all(&[ch('a'), key(KeyCode::Up), ch('b')]),
            vec![b'a', b'b']
        );
    }

    #[test]
    fn enqueue_macro_pushes_decoded_keys_within_budget() {
        let mut q = VecDeque::new();
        let mut budget = 0;
        // A top-level @ (empty queue, no budget) seeds MACRO_KEY_CAP, then enqueues "ax".
        assert!(enqueue_macro(&mut q, &mut budget, b"ax"));
        assert_eq!(q, VecDeque::from(vec![ch('a'), ch('x')]));
        assert_eq!(
            budget,
            MACRO_KEY_CAP - 2,
            "budget decremented by keys pushed"
        );
    }

    #[test]
    fn enqueue_macro_truncates_at_the_recursion_cap() {
        let mut q = VecDeque::new();
        // Pretend we are deep in an expansion with only 3 keys of budget left.
        let mut budget = 3;
        q.push_back(ch('_')); // non-empty queue ⇒ do NOT reseed the budget
        let full = enqueue_macro(&mut q, &mut budget, b"abcde"); // 5 keys, only 3 allowed
        assert!(!full, "reports truncation");
        assert_eq!(budget, 0, "budget exhausted");
        // The 3 taken keys were appended after the sentinel.
        assert_eq!(q.len(), 1 + 3);
    }
}
