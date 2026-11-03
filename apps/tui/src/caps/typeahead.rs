//! The keys typed while the startup probe owned the terminal (#1013).
//!
//! The probe ([`super::probe`]) reads stdin itself until the DA1 fence, before crossterm's event reader
//! exists, so whatever the user typed in that window passes through the probe's reads. The probe keeps
//! those bytes ([`super::probe::ProbeParser::take_typed`]); [`decode`] turns them into the key events
//! crossterm would have reported for them, so the event loop reads them first, in order, instead of
//! losing them. RFC-0005: the parser separates replies from input and buffers rather than drops.
//!
//! The probe runs before any keyboard protocol is pushed, so the bytes are legacy encoding: this mirrors
//! crossterm 0.29's legacy parser (`event/sys/unix/parse.rs`) for keys. A sequence it does not know
//! (mouse reports cannot occur yet) is skipped.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// The keys `bytes` encode, in order. Never panics; an unknown sequence or a malformed byte is skipped.
#[must_use]
pub fn decode(bytes: &[u8]) -> Vec<KeyEvent> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let (key, width) = one(bytes, i);
        out.extend(key);
        i += width.max(1);
    }
    out
}

/// The key at `bytes[i]` and how many bytes it took.
fn one(bytes: &[u8], i: usize) -> (Option<KeyEvent>, usize) {
    let key = |code, m| Some(KeyEvent::new(code, m));
    match bytes[i] {
        0x1b => match bytes.get(i + 1) {
            None | Some(0x1b) => (key(KeyCode::Esc, KeyModifiers::NONE), 1),
            Some(b'[') => csi(bytes, i),
            Some(b'O') => match bytes.get(i + 2) {
                Some(&b) => (ss3(b).and_then(|c| key(c, KeyModifiers::NONE)), 3),
                None => (key(KeyCode::Esc, KeyModifiers::NONE), 1),
            },
            // `ESC x` is Alt-x, as crossterm reads it.
            Some(_) => {
                let (k, w) = one(bytes, i + 1);
                (
                    k.map(|mut k| {
                        k.modifiers |= KeyModifiers::ALT;
                        k
                    }),
                    1 + w,
                )
            }
        },
        b'\r' => (key(KeyCode::Enter, KeyModifiers::NONE), 1),
        b'\t' => (key(KeyCode::Tab, KeyModifiers::NONE), 1),
        0x7f => (key(KeyCode::Backspace, KeyModifiers::NONE), 1),
        // `0x0a` is CTRL-J in raw mode, as crossterm reads it.
        c @ 0x01..=0x1a => (
            key(KeyCode::Char((c - 1 + b'a') as char), KeyModifiers::CONTROL),
            1,
        ),
        c @ 0x1c..=0x1f => (
            key(
                KeyCode::Char((c - 0x1c + b'4') as char),
                KeyModifiers::CONTROL,
            ),
            1,
        ),
        0x00 => (key(KeyCode::Char(' '), KeyModifiers::CONTROL), 1),
        lead => {
            let width = match lead {
                0xc0..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf7 => 4,
                _ => 1,
            };
            let end = (i + width).min(bytes.len());
            match std::str::from_utf8(&bytes[i..end])
                .ok()
                .and_then(|s| s.chars().next())
            {
                Some(c) => {
                    let m = if c.is_uppercase() {
                        KeyModifiers::SHIFT
                    } else {
                        KeyModifiers::NONE
                    };
                    (key(KeyCode::Char(c), m), c.len_utf8())
                }
                None => (None, 1),
            }
        }
    }
}

/// `ESC O x` (application cursor and keypad keys).
fn ss3(b: u8) -> Option<KeyCode> {
    Some(match b {
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'C' => KeyCode::Right,
        b'D' => KeyCode::Left,
        b'H' => KeyCode::Home,
        b'F' => KeyCode::End,
        b'P'..=b'S' => KeyCode::F(1 + b - b'P'),
        _ => return None,
    })
}

/// `ESC [ <params> <final>` starting at `bytes[i]`: a cursor key (`CSI A`, `CSI 1;5C`) or a `~` key
/// (`CSI 3~`, `CSI 15;2~`). An unfinished sequence is taken whole and yields nothing.
fn csi(bytes: &[u8], i: usize) -> (Option<KeyEvent>, usize) {
    let mut j = i + 2;
    while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
        j += 1;
    }
    let width = (j + 1).min(bytes.len()) - i;
    let Some(&fin) = bytes.get(j) else {
        return (None, width);
    };
    let params: Vec<u16> = bytes[i + 2..j]
        .split(|&b| b == b';')
        .map(|p| {
            std::str::from_utf8(p)
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0)
        })
        .collect();
    let mods = modifiers(params.get(1).copied().unwrap_or(1));
    let code = match fin {
        b'A' => Some(KeyCode::Up),
        b'B' => Some(KeyCode::Down),
        b'C' => Some(KeyCode::Right),
        b'D' => Some(KeyCode::Left),
        b'H' => Some(KeyCode::Home),
        b'F' => Some(KeyCode::End),
        b'P'..=b'S' => Some(KeyCode::F(1 + fin - b'P')),
        b'Z' => {
            return (
                Some(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)),
                width,
            )
        }
        b'~' => match params.first().copied().unwrap_or(0) {
            1 | 7 => Some(KeyCode::Home),
            2 => Some(KeyCode::Insert),
            3 => Some(KeyCode::Delete),
            4 | 8 => Some(KeyCode::End),
            5 => Some(KeyCode::PageUp),
            6 => Some(KeyCode::PageDown),
            n @ 11..=15 => Some(KeyCode::F((n - 10) as u8)),
            n @ 17..=21 => Some(KeyCode::F((n - 11) as u8)),
            n @ 23..=26 => Some(KeyCode::F((n - 12) as u8)),
            n @ 28..=29 => Some(KeyCode::F((n - 15) as u8)),
            n @ 31..=34 => Some(KeyCode::F((n - 17) as u8)),
            _ => None,
        },
        _ => None,
    };
    (code.map(|c| KeyEvent::new(c, mods)), width)
}

/// The xterm modifier parameter (`1 + bits`: 1 Shift, 2 Alt, 4 Control).
fn modifiers(param: u16) -> KeyModifiers {
    let bits = param.saturating_sub(1);
    let mut m = KeyModifiers::NONE;
    if bits & 1 != 0 {
        m |= KeyModifiers::SHIFT;
    }
    if bits & 2 != 0 {
        m |= KeyModifiers::ALT;
    }
    if bits & 4 != 0 {
        m |= KeyModifiers::CONTROL;
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, m)
    }

    #[test]
    fn text_controls_and_cursor_keys_decode_as_crossterm_reads_them() {
        let none = KeyModifiers::NONE;
        assert_eq!(
            decode(b"iH\x1b\r\t\x7f\x17\x1d\x00"),
            vec![
                k(KeyCode::Char('i'), none),
                k(KeyCode::Char('H'), KeyModifiers::SHIFT),
                // `ESC CR` is Alt-Enter.
                k(KeyCode::Enter, KeyModifiers::ALT),
                k(KeyCode::Tab, none),
                k(KeyCode::Backspace, none),
                k(KeyCode::Char('w'), KeyModifiers::CONTROL),
                k(KeyCode::Char('5'), KeyModifiers::CONTROL),
                k(KeyCode::Char(' '), KeyModifiers::CONTROL),
            ]
        );
        assert_eq!(
            decode(b"\x1b[A\x1b[1;5C\x1b[3~\x1b[15;2~\x1bOP\x1b[Z"),
            vec![
                k(KeyCode::Up, none),
                k(KeyCode::Right, KeyModifiers::CONTROL),
                k(KeyCode::Delete, none),
                k(KeyCode::F(5), KeyModifiers::SHIFT),
                k(KeyCode::F(1), none),
                k(KeyCode::BackTab, KeyModifiers::SHIFT),
            ]
        );
        assert_eq!(
            decode("한\x1bx\x1b".as_bytes()),
            vec![
                k(KeyCode::Char('한'), none),
                k(KeyCode::Char('x'), KeyModifiers::ALT),
                k(KeyCode::Esc, none),
            ]
        );
    }

    #[test]
    fn unknown_or_cut_sequences_are_skipped_without_panicking() {
        assert_eq!(
            decode(b"\x1b[200~a"),
            vec![k(KeyCode::Char('a'), KeyModifiers::NONE)]
        );
        assert_eq!(decode(b"\x1b[1;5"), vec![]);
        assert_eq!(decode(b"\xff\xc3"), vec![]);
    }
}
