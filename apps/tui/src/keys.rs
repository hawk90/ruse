//! The macro key codec (D-055): a round-trippable `KeyEvent` ↔ bytes encoding. A recorded Vim macro is the
//! RAW keystroke stream stored as bytes in a register, so `encode` mirrors what a terminal delivers (and what
//! `pty::encode_key` forwards — but that one is lossy/one-way) and `decode` inverts it. The slice-1 alphabet
//! is printable UTF-8 + `Esc`/`Enter`/`Tab`/`Backspace` + `Ctrl-a..z` + the navigation keys (arrows,
//! Home/End, PageUp/Down, Delete/Insert, BackTab) via a reserved `0x80`+tag prefix. Fn/Alt/Shift-specials are
//! still DEFERRED (they encode to nothing, and `decode` skips bytes it does not recognise, so a hand-edited
//! register still runs).

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

/// The macro record/replay state machine (D-055), extracted from the session loop so the full
/// record→stop→replay cycle is unit-testable without driving the terminal. It owns the recording buffer,
/// the two prefix flags (`q`/`@`), the replay key-queue, and the recursion budget; the session calls
/// [`MacroState::step`] for every key and [`MacroState::next_replay`] to drain queued keys before reading
/// the terminal. Register I/O stays OUTSIDE (it lives in the core `Workspace`) — [`Step::Store`] /
/// [`Step::Replay`] hand the register name back to the caller.
#[derive(Default)]
pub struct MacroState {
    recording: Option<(char, Vec<KeyEvent>)>, // register name (case preserved: UPPER = append on stop)
    pending_q: bool,  // `q` armed: the next key names the register to record INTO
    pending_at: bool, // `@` armed: the next key names the register to REPLAY
    last_played: Option<char>, // the last register replayed, for `@@`
    queue: VecDeque<KeyEvent>,
    budget: u32,
}

/// What the session should do with a key after [`MacroState::step`] has processed it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Step {
    /// Dispatch this key normally (engine + intercepts). It was also captured if a recording is active.
    Dispatch(KeyEvent),
    /// The macro layer consumed the key (a `q`/`@` prefix arm, or a register-name that started recording).
    Consumed,
    /// Recording just stopped — store these bytes into register `char`, then move on.
    Store(char, Vec<u8>),
    /// `@{char}` — the caller reads register `char`'s bytes and passes them to [`MacroState::replay`].
    Replay(char),
}

fn is_bare(key: KeyEvent, c: char) -> bool {
    key.code == KeyCode::Char(c) && key.modifiers.is_empty()
}

impl MacroState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a recording is currently in progress (`q{reg}` seen, no stopping `q` yet).
    #[must_use]
    pub fn is_recording(&self) -> bool {
        self.recording.is_some()
    }

    /// Pop the next queued replay key, or `None` when the queue is empty. Draining to empty resets the
    /// recursion budget, so the NEXT top-level `@` starts fresh. The session calls this before reading the
    /// terminal; a `None` key came from the terminal (a *typed* key).
    pub fn next_replay(&mut self) -> Option<KeyEvent> {
        let k = self.queue.pop_front();
        if k.is_none() {
            self.budget = 0;
        }
        k
    }

    /// Enqueue register bytes for replay, honouring the shared recursion budget. Returns `false` if the
    /// macro was TRUNCATED at [`MACRO_KEY_CAP`] (the caller shows a status).
    pub fn replay(&mut self, bytes: &[u8]) -> bool {
        enqueue_macro(&mut self.queue, &mut self.budget, bytes)
    }

    /// Process one key. `from_replay` is true for a key drained from the replay queue (never re-recorded —
    /// Vim records `@x`, not its expansion); `normal` is whether the focused view is in Normal mode with no
    /// command-line open (the `q`/`@` prefixes only arm there).
    pub fn step(&mut self, key: KeyEvent, from_replay: bool, normal: bool) -> Step {
        // A typed bare `q` STOPS a live recording — unless it is the register-name for a pending `q`/`@`
        // (e.g. `@q` while recording runs macro q; it must not be read as a stop).
        if !from_replay && !self.pending_q && !self.pending_at {
            if let Some((reg, buf)) = self.recording.as_ref() {
                if is_bare(key, 'q') {
                    let bytes = encode_all(buf);
                    let reg = *reg;
                    self.recording = None;
                    return Step::Store(reg, bytes);
                }
            }
        }
        // Capture every other typed key while recording (including a `@x` typed mid-recording).
        if !from_replay {
            if let Some((_, buf)) = self.recording.as_mut() {
                buf.push(key);
            }
        }
        // Register-name after `q` — start recording into it. A lowercase name overwrites, an UPPERCASE name
        // appends (`qA` extends macro a); the case is preserved and honoured at [`Step::Store`] time. A
        // non-letter aborts the arm.
        if self.pending_q {
            self.pending_q = false;
            if let KeyCode::Char(c) = key.code {
                if c.is_ascii_alphabetic() {
                    self.recording = Some((c, Vec::new()));
                }
            }
            return Step::Consumed;
        }
        // Register-name after `@` — resolve the register (any-case letter → lowercase slot; `@@` repeats the
        // last macro) and hand it back so the caller reads + replays it.
        if self.pending_at {
            self.pending_at = false;
            let target = match key.code {
                KeyCode::Char(c) if c.is_ascii_alphabetic() => Some(c.to_ascii_lowercase()),
                KeyCode::Char('@') => self.last_played, // `@@` — repeat the last-played macro
                _ => None,
            };
            return match target {
                Some(c) => {
                    self.last_played = Some(c);
                    Step::Replay(c)
                }
                None => Step::Consumed,
            };
        }
        // Arm the prefixes (Normal only). `q` only arms when NOT recording (a recording `q` stopped above).
        if normal && self.recording.is_none() && is_bare(key, 'q') {
            self.pending_q = true;
            return Step::Consumed;
        }
        if normal && is_bare(key, '@') {
            self.pending_at = true;
            return Step::Consumed;
        }
        Step::Dispatch(key)
    }
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
        // Navigation keys ride a reserved `0x80` prefix + a tag byte. `0x80` is an invalid UTF-8 lead, so
        // an older `decode` already skipped it — this encoding is backward-compatible. Fn/Alt/Shift-specials
        // have no tag and fall out as `None` (still deferred).
        code => vec![0x80, special_tag(code)?],
    };
    Some(bytes)
}

/// The `0x80`-prefix tag byte for a navigation key, or `None` for a key not in the macro alphabet.
/// Paired with [`special_from_tag`] as the inverse.
fn special_tag(code: KeyCode) -> Option<u8> {
    Some(match code {
        KeyCode::Up => b'A',
        KeyCode::Down => b'B',
        KeyCode::Right => b'C',
        KeyCode::Left => b'D',
        KeyCode::Home => b'H',
        KeyCode::End => b'F',
        KeyCode::PageUp => b'5',
        KeyCode::PageDown => b'6',
        KeyCode::Delete => b'3',
        KeyCode::Insert => b'2',
        KeyCode::BackTab => b'Z',
        _ => return None,
    })
}

/// The navigation [`KeyCode`] for a `0x80`-prefix tag byte, or `None` for an unknown tag (skipped on decode).
fn special_from_tag(tag: u8) -> Option<KeyCode> {
    Some(match tag {
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'C' => KeyCode::Right,
        b'D' => KeyCode::Left,
        b'H' => KeyCode::Home,
        b'F' => KeyCode::End,
        b'5' => KeyCode::PageUp,
        b'6' => KeyCode::PageDown,
        b'3' => KeyCode::Delete,
        b'2' => KeyCode::Insert,
        b'Z' => KeyCode::BackTab,
        _ => return None,
    })
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
            0x80 => {
                // A navigation key: `0x80` + tag. Consume both; an unknown/absent tag is skipped.
                if let Some(code) = bytes.get(i + 1).copied().and_then(special_from_tag) {
                    out.push(plain(code));
                }
                i += 2;
                continue;
            }
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
    fn navigation_keys_round_trip_via_the_0x80_prefix() {
        let nav = [
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Home,
            KeyCode::End,
            KeyCode::PageUp,
            KeyCode::PageDown,
            KeyCode::Delete,
            KeyCode::Insert,
            KeyCode::BackTab,
        ];
        for code in nav {
            let b = encode(key(code)).expect("nav key encodes");
            assert_eq!(b[0], 0x80, "reserved prefix");
            assert_eq!(decode(&b), vec![key(code)], "round-trip {code:?}");
        }
        // Mixed with printable text, in order.
        let seq = vec![ch('a'), key(KeyCode::Left), ch('b')];
        assert_eq!(decode(&encode_all(&seq)), seq);
    }

    #[test]
    fn unencodable_keys_are_still_dropped() {
        // Fn / Alt / etc. stay deferred: encoded to nothing, skipped by encode_all.
        assert_eq!(encode(key(KeyCode::F(5))), None);
        assert_eq!(
            encode_all(&[ch('a'), key(KeyCode::F(5)), ch('b')]),
            vec![b'a', b'b']
        );
        // A stray/trailing 0x80 with no tag decodes to nothing (no panic).
        assert_eq!(decode(&[0x80]), vec![]);
        assert_eq!(decode(&[b'x', 0x80, b'?']), vec![ch('x')]); // unknown tag skipped
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
    fn full_record_stop_replay_cycle() {
        use std::collections::HashMap;
        let mut m = MacroState::new();
        let mut regs: HashMap<char, Vec<u8>> = HashMap::new();

        // Record `q a  x x  q` — arm, start into reg a, capture two x, stop.
        assert_eq!(m.step(ch('q'), false, true), Step::Consumed);
        assert!(
            !m.is_recording(),
            "q only arms; recording starts on the register name"
        );
        assert_eq!(m.step(ch('a'), false, true), Step::Consumed);
        assert!(m.is_recording());
        assert_eq!(
            m.step(ch('x'), false, true),
            Step::Dispatch(ch('x')),
            "captured AND dispatched"
        );
        assert_eq!(m.step(ch('x'), false, true), Step::Dispatch(ch('x')));
        match m.step(ch('q'), false, true) {
            Step::Store(reg, bytes) => {
                regs.insert(reg, bytes);
            }
            other => panic!("expected Store, got {other:?}"),
        }
        assert!(!m.is_recording());
        assert_eq!(
            regs[&'a'], b"xx",
            "the two captured x's are stored as bytes"
        );

        // Replay `@ a` — arm, resolve the register, enqueue; keys drain as `from_replay`.
        assert_eq!(m.step(ch('@'), false, true), Step::Consumed);
        match m.step(ch('a'), false, true) {
            Step::Replay(reg) => assert!(m.replay(&regs[&reg])),
            other => panic!("expected Replay, got {other:?}"),
        }
        assert_eq!(m.next_replay(), Some(ch('x')));
        assert_eq!(m.next_replay(), Some(ch('x')));
        assert_eq!(m.next_replay(), None, "queue drained");
    }

    #[test]
    fn insert_macro_round_trips_through_the_state_machine() {
        let mut m = MacroState::new();
        // `q b  i Z <Esc>  q`
        m.step(ch('q'), false, true);
        m.step(ch('b'), false, true);
        for k in [ch('i'), ch('Z'), key(KeyCode::Esc)] {
            assert!(matches!(m.step(k, false, true), Step::Dispatch(_)));
        }
        let bytes = match m.step(ch('q'), false, true) {
            Step::Store(_, b) => b,
            other => panic!("expected Store, got {other:?}"),
        };
        assert_eq!(bytes, vec![b'i', b'Z', 0x1b]);
        assert_eq!(decode(&bytes), vec![ch('i'), ch('Z'), key(KeyCode::Esc)]);
    }

    #[test]
    fn replayed_keys_are_not_re_recorded_and_at_x_records_literally() {
        let mut m = MacroState::new();
        // Start recording into a, then type `@b` mid-record: both keys captured literally (not expanded).
        m.step(ch('q'), false, true);
        m.step(ch('a'), false, true);
        assert_eq!(
            m.step(ch('@'), false, true),
            Step::Consumed,
            "@ arms (and is captured)"
        );
        assert_eq!(
            m.step(ch('b'), false, true),
            Step::Replay('b'),
            "b resolves the @, still captured"
        );
        // A key arriving from the replay queue is NOT captured into the recording.
        assert!(matches!(m.step(ch('y'), true, true), Step::Dispatch(_)));
        let bytes = match m.step(ch('q'), false, true) {
            Step::Store(_, b) => b,
            other => panic!("expected Store, got {other:?}"),
        };
        assert_eq!(
            bytes, b"@b",
            "the literal @b was recorded; the replayed y was not"
        );
    }

    #[test]
    fn at_at_repeats_the_last_played_macro() {
        let mut m = MacroState::new();
        m.step(ch('@'), false, true);
        assert_eq!(
            m.step(ch('a'), false, true),
            Step::Replay('a'),
            "@a plays a"
        );
        m.step(ch('@'), false, true);
        assert_eq!(
            m.step(ch('@'), false, true),
            Step::Replay('a'),
            "@@ repeats a"
        );
    }

    #[test]
    fn at_at_before_any_play_is_a_noop() {
        let mut m = MacroState::new();
        m.step(ch('@'), false, true);
        assert_eq!(
            m.step(ch('@'), false, true),
            Step::Consumed,
            "nothing to repeat yet"
        );
    }

    #[test]
    fn uppercase_q_records_for_append_and_uppercase_at_reads_lowercase() {
        let mut m = MacroState::new();
        m.step(ch('q'), false, true);
        m.step(ch('A'), false, true); // record into A → append to a
        assert!(m.is_recording());
        m.step(ch('x'), false, true);
        match m.step(ch('q'), false, true) {
            Step::Store(reg, bytes) => {
                assert_eq!(
                    reg, 'A',
                    "append is signalled by the uppercase register name"
                );
                assert_eq!(bytes, b"x");
            }
            other => panic!("expected Store, got {other:?}"),
        }
        // `@A` plays the lowercase slot a.
        m.step(ch('@'), false, true);
        assert_eq!(m.step(ch('A'), false, true), Step::Replay('a'));
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
